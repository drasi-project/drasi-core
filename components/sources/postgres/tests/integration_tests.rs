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

mod postgres_helpers;

use anyhow::Result;
use drasi_bootstrap_postgres::{
    PostgresBootstrapConfig, PostgresBootstrapProvider, SslMode as BootstrapSslMode,
    TableKeyConfig as BootstrapTableKeyConfig,
};
use drasi_lib::{config::SourceSubscriptionSettings, DrasiLib, Query, Source, SourceError};
use drasi_reaction_application::{ApplicationReaction, ApplicationReactionHandle};
use drasi_source_postgres::{
    PostgresReplicationSource, PostgresSourceConfig, SslMode, TableKeyConfig,
};
use postgres_helpers::{
    create_decimal_test_table, create_logical_replication_slot, create_publication,
    create_test_table, create_test_table_replica_identity_default, delete_test_row,
    get_slot_confirmed_flush_lsn, grant_replication, grant_table_access, insert_decimal_test_row,
    insert_test_row, setup_replication_postgres, update_test_row,
};
use serial_test::serial;
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TEST_TABLE: &str = "users";
const TEST_PUBLICATION: &str = "drasi_test_pub";

fn init_logging() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();
}

fn slot_name() -> String {
    format!("drasi_slot_{}", uuid::Uuid::new_v4().simple())
}

async fn wait_for_query_results(
    core: &Arc<DrasiLib>,
    query_id: &str,
    predicate: impl Fn(&[serde_json::Value]) -> bool,
) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(20);
    loop {
        // Try to get results, but don't fail if query isn't running yet
        // (it might still be completing bootstrap)
        match core.get_query_results(query_id).await {
            Ok(results) => {
                if predicate(&results) {
                    return Ok(());
                }
            }
            Err(e) if e.to_string().contains("is not running") => {
                // Query isn't running yet, keep waiting
            }
            Err(e) => return Err(e.into()),
        }

        if start.elapsed() > timeout {
            anyhow::bail!("Timed out waiting for query results for query_id `{query_id}`")
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn build_core(
    config: &postgres_helpers::ReplicationPostgresConfig,
    slot_name: String,
) -> Result<(Arc<DrasiLib>, ApplicationReactionHandle)> {
    let source_config = build_source_config(config, slot_name);

    let bootstrap_config = PostgresBootstrapConfig {
        host: source_config.host.clone(),
        port: source_config.port,
        database: source_config.database.clone(),
        user: source_config.user.clone(),
        password: source_config.password.clone(),
        tables: source_config.tables.clone(),
        slot_name: source_config.slot_name.clone(),
        publication_name: source_config.publication_name.clone(),
        ssl_mode: BootstrapSslMode::Disable,
        table_keys: vec![BootstrapTableKeyConfig {
            table: TEST_TABLE.to_string(),
            key_columns: vec!["id".to_string()],
        }],
    };

    let bootstrap_provider = PostgresBootstrapProvider::new(bootstrap_config);

    let source = PostgresReplicationSource::builder("pg-test-source")
        .with_config(source_config)
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    let query = Query::cypher("test-query")
        .query(
            r#"
            MATCH (u:users)
            RETURN u.id AS id, u.name AS name
            "#,
        )
        .from_source("pg-test-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let (reaction, handle) = ApplicationReaction::builder("test-reaction")
        .with_query("test-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("pg-test-core")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    );

    Ok((core, handle))
}

fn build_source_config(
    config: &postgres_helpers::ReplicationPostgresConfig,
    slot_name: String,
) -> PostgresSourceConfig {
    PostgresSourceConfig {
        host: config.host.clone(),
        port: config.port,
        database: config.database.clone(),
        user: config.user.clone(),
        password: config.password.clone(),
        tables: vec![TEST_TABLE.to_string()],
        slot_name,
        publication_name: TEST_PUBLICATION.to_string(),
        ssl_mode: SslMode::Disable,
        table_keys: vec![TableKeyConfig {
            table: TEST_TABLE.to_string(),
            key_columns: vec!["id".to_string()],
        }],
    }
}

fn build_source(
    config: &postgres_helpers::ReplicationPostgresConfig,
    slot_name: String,
) -> Result<PostgresReplicationSource> {
    PostgresReplicationSource::builder("pg-direct-source")
        .with_config(build_source_config(config, slot_name))
        .build()
}

fn subscription_settings(
    source_id: &str,
    query_id: &str,
    resume_from: Option<u64>,
    request_position_handle: bool,
) -> SourceSubscriptionSettings {
    SourceSubscriptionSettings {
        source_id: source_id.to_string(),
        enable_bootstrap: false,
        query_id: query_id.to_string(),
        nodes: HashSet::new(),
        relations: HashSet::new(),
        resume_from,
        request_position_handle,
    }
}

async fn wait_for_source_event(
    response: &mut drasi_lib::channels::SubscriptionResponse,
) -> Result<Arc<drasi_lib::channels::SourceEventWrapper>> {
    tokio::time::timeout(Duration::from_secs(15), response.receiver.recv()).await?
}

async fn wait_for_slot_confirmed_flush_lsn(
    client: &tokio_postgres::Client,
    slot_name: &str,
    predicate: impl Fn(Option<u64>) -> bool,
) -> Result<Option<u64>> {
    let start = Instant::now();
    let timeout = Duration::from_secs(20);

    loop {
        let lsn = get_slot_confirmed_flush_lsn(client, slot_name).await?;
        if predicate(lsn) {
            return Ok(lsn);
        }

        if start.elapsed() > timeout {
            anyhow::bail!(
                "Timed out waiting for confirmed_flush_lsn predicate for slot `{slot_name}`"
            );
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_source_connects_and_starts() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let slot_name = slot_name();
    create_logical_replication_slot(&client, &slot_name).await?;

    let (core, _handle) = build_core(pg.config(), slot_name).await?;
    core.start().await?;

    wait_for_query_results(&core, "test-query", |results| results.is_empty()).await?;
    let status = core.get_source_status("pg-test-source").await?;
    assert_eq!(status, drasi_lib::channels::ComponentStatus::Running);

    core.stop().await?;
    pg.cleanup().await;

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_insert_detection() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let slot_name = slot_name();
    create_logical_replication_slot(&client, &slot_name).await?;

    let (core, _handle) = build_core(pg.config(), slot_name).await?;
    core.start().await?;
    wait_for_query_results(&core, "test-query", |results| results.is_empty()).await?;

    insert_test_row(&client, TEST_TABLE, 1, "Alice").await?;

    wait_for_query_results(&core, "test-query", |results| {
        results
            .iter()
            .any(|row| row.get("name") == Some(&"Alice".into()))
    })
    .await?;

    core.stop().await?;
    pg.cleanup().await;

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_update_detection() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let slot_name = slot_name();
    create_logical_replication_slot(&client, &slot_name).await?;

    let (core, _handle) = build_core(pg.config(), slot_name).await?;
    core.start().await?;
    wait_for_query_results(&core, "test-query", |results| results.is_empty()).await?;

    insert_test_row(&client, TEST_TABLE, 1, "Alice").await?;
    wait_for_query_results(&core, "test-query", |results| {
        results
            .iter()
            .any(|row| row.get("name") == Some(&"Alice".into()))
    })
    .await?;

    update_test_row(&client, TEST_TABLE, 1, "Alice Updated").await?;
    wait_for_query_results(&core, "test-query", |results| {
        results
            .iter()
            .any(|row| row.get("name") == Some(&"Alice Updated".into()))
    })
    .await?;

    core.stop().await?;
    pg.cleanup().await;

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_update_without_old_tuple_stays_update() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table_replica_identity_default(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let slot_name = slot_name();
    create_logical_replication_slot(&client, &slot_name).await?;

    let (core, _handle) = build_core(pg.config(), slot_name).await?;
    core.start().await?;
    wait_for_query_results(&core, "test-query", |results| results.is_empty()).await?;

    insert_test_row(&client, TEST_TABLE, 1, "Alice").await?;
    wait_for_query_results(&core, "test-query", |results| {
        results
            .iter()
            .any(|row| row.get("name") == Some(&"Alice".into()))
    })
    .await?;

    update_test_row(&client, TEST_TABLE, 1, "Alice Updated").await?;
    wait_for_query_results(&core, "test-query", |results| {
        results.len() == 1
            && results
                .iter()
                .any(|row| row.get("name") == Some(&"Alice Updated".into()))
    })
    .await?;

    core.stop().await?;
    pg.cleanup().await;

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_delete_detection() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let slot_name = slot_name();
    create_logical_replication_slot(&client, &slot_name).await?;

    let (core, _handle) = build_core(pg.config(), slot_name).await?;
    core.start().await?;
    wait_for_query_results(&core, "test-query", |results| results.is_empty()).await?;

    insert_test_row(&client, TEST_TABLE, 1, "Alice").await?;
    wait_for_query_results(&core, "test-query", |results| {
        results
            .iter()
            .any(|row| row.get("name") == Some(&"Alice".into()))
    })
    .await?;

    delete_test_row(&client, TEST_TABLE, 1).await?;
    wait_for_query_results(&core, "test-query", |results| results.is_empty()).await?;

    core.stop().await?;
    pg.cleanup().await;

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_full_crud_cycle() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let slot_name = slot_name();
    create_logical_replication_slot(&client, &slot_name).await?;

    let (core, _handle) = build_core(pg.config(), slot_name).await?;
    core.start().await?;
    wait_for_query_results(&core, "test-query", |results| results.is_empty()).await?;

    insert_test_row(&client, TEST_TABLE, 1, "Alice").await?;
    wait_for_query_results(&core, "test-query", |results| {
        results
            .iter()
            .any(|row| row.get("name") == Some(&"Alice".into()))
    })
    .await?;

    update_test_row(&client, TEST_TABLE, 1, "Alice Updated").await?;
    wait_for_query_results(&core, "test-query", |results| {
        results
            .iter()
            .any(|row| row.get("name") == Some(&"Alice Updated".into()))
    })
    .await?;

    delete_test_row(&client, TEST_TABLE, 1).await?;
    wait_for_query_results(&core, "test-query", |results| results.is_empty()).await?;

    core.stop().await?;
    pg.cleanup().await;

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_confirmed_flush_lsn_stays_pinned_without_subscribers() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let slot_name = slot_name();
    create_logical_replication_slot(&client, &slot_name).await?;

    let source = build_source(pg.config(), slot_name.clone())?;
    source.start().await?;
    let initial = get_slot_confirmed_flush_lsn(&client, &slot_name).await?;

    insert_test_row(&client, TEST_TABLE, 1, "Alice").await?;
    let after_first = get_slot_confirmed_flush_lsn(&client, &slot_name).await?;

    insert_test_row(&client, TEST_TABLE, 2, "Bob").await?;
    let after_second = get_slot_confirmed_flush_lsn(&client, &slot_name).await?;

    assert_eq!(
        after_first, initial,
        "confirmed_flush_lsn should remain unchanged after the first insert with no subscribers"
    );
    assert_eq!(
        after_second, initial,
        "confirmed_flush_lsn should stay pinned when no subscriber position handles are active"
    );

    source.stop().await?;
    pg.cleanup().await;

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_resume_from_retained_lsn_replays_sequence_stamped_events() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let slot_name = slot_name();
    create_logical_replication_slot(&client, &slot_name).await?;

    let source = build_source(pg.config(), slot_name.clone())?;
    source.start().await?;

    let mut sub1 = source
        .subscribe(subscription_settings(source.id(), "q1", None, true))
        .await?;
    let handle1 = sub1
        .position_handle
        .as_ref()
        .expect("expected position handle for replay-capable subscription")
        .clone();

    insert_test_row(&client, TEST_TABLE, 1, "Alice").await?;
    let event1 = wait_for_source_event(&mut sub1).await?;
    let seq1 = event1
        .sequence
        .expect("expected WAL sequence on first event");
    handle1.store(seq1, Ordering::Relaxed);

    insert_test_row(&client, TEST_TABLE, 2, "Bob").await?;
    let event2 = wait_for_source_event(&mut sub1).await?;
    let seq2 = event2
        .sequence
        .expect("expected WAL sequence on second event");
    assert!(seq2 >= seq1, "expected monotonic WAL sequences");
    handle1.store(seq2, Ordering::Relaxed);

    let mut resumed = source
        .subscribe(subscription_settings(source.id(), "q2", Some(seq1), true))
        .await?;

    let replayed = wait_for_source_event(&mut resumed).await?;
    let replay_seq = replayed
        .sequence
        .expect("expected sequence on replayed event");
    assert!(
        replay_seq >= seq1 && replay_seq <= seq2,
        "expected replayed event from retained WAL range, got {replay_seq:x} outside [{seq1:x}, {seq2:x}]"
    );

    insert_test_row(&client, TEST_TABLE, 3, "Carol").await?;
    let live = wait_for_source_event(&mut resumed).await?;
    let live_seq = live.sequence.expect("expected sequence on live event");
    assert!(
        live_seq >= replay_seq,
        "expected subsequent live event sequence to be >= replayed event sequence"
    );

    source.stop().await?;
    pg.cleanup().await;

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_resume_subscription_replays_before_new_live_events() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let slot_name = slot_name();
    create_logical_replication_slot(&client, &slot_name).await?;

    let source = Arc::new(build_source(pg.config(), slot_name.clone())?);
    source.start().await?;

    let mut sub1 = source
        .subscribe(subscription_settings(source.id(), "q1", None, true))
        .await?;
    let handle1 = sub1
        .position_handle
        .as_ref()
        .expect("expected position handle for replay-capable subscription")
        .clone();

    insert_test_row(&client, TEST_TABLE, 1, "Alice").await?;
    let event1 = wait_for_source_event(&mut sub1).await?;
    let seq1 = event1
        .sequence
        .expect("expected WAL sequence on first event");
    handle1.store(seq1, Ordering::Relaxed);

    insert_test_row(&client, TEST_TABLE, 2, "Bob").await?;
    let event2 = wait_for_source_event(&mut sub1).await?;
    let seq2 = event2
        .sequence
        .expect("expected WAL sequence on second event");
    handle1.store(seq2, Ordering::Relaxed);

    let source_for_resume = source.clone();
    let source_id = source.id().to_string();
    let subscribe_task = tokio::spawn(async move {
        source_for_resume
            .subscribe(subscription_settings(&source_id, "q2", Some(seq1), true))
            .await
    });

    tokio::task::yield_now().await;

    let mut inserted_during_resume = false;
    for next_id in 3..=12 {
        if subscribe_task.is_finished() {
            break;
        }

        insert_test_row(&client, TEST_TABLE, next_id, &format!("User {next_id}")).await?;
        inserted_during_resume = true;
        tokio::task::yield_now().await;
    }

    assert!(
        inserted_during_resume,
        "expected at least one live insert while the replaying subscription was pending"
    );

    let mut resumed = subscribe_task.await??;

    let first = wait_for_source_event(&mut resumed).await?;
    let first_seq = first
        .sequence
        .expect("expected sequence on first resumed event");
    assert!(
        first_seq <= seq2,
        "expected resumed subscriber to receive replay backlog before newer live events, got {first_seq:x} > {seq2:x}"
    );

    source.stop().await?;
    pg.cleanup().await;

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_resume_from_before_slot_watermark_returns_position_unavailable() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let slot_name = slot_name();
    create_logical_replication_slot(&client, &slot_name).await?;

    let source = build_source(pg.config(), slot_name.clone())?;
    source.start().await?;

    let mut sub = source
        .subscribe(subscription_settings(
            source.id(),
            "watermark-q",
            None,
            true,
        ))
        .await?;
    let handle = sub
        .position_handle
        .as_ref()
        .expect("expected position handle")
        .clone();

    insert_test_row(&client, TEST_TABLE, 1, "Alice").await?;
    let event1 = wait_for_source_event(&mut sub).await?;
    let seq1 = event1.sequence.expect("expected first sequence");
    handle.store(seq1, Ordering::Relaxed);

    insert_test_row(&client, TEST_TABLE, 2, "Bob").await?;
    let event2 = wait_for_source_event(&mut sub).await?;
    let seq2 = event2.sequence.expect("expected second sequence");
    handle.store(seq2, Ordering::Relaxed);

    let earliest = wait_for_slot_confirmed_flush_lsn(
        &client,
        &slot_name,
        |lsn| matches!(lsn, Some(value) if value >= seq2),
    )
    .await?
    .expect("expected confirmed_flush_lsn after feedback");
    assert!(earliest > 0, "expected a real slot watermark");

    let requested = earliest.saturating_sub(1);
    let err = match source
        .subscribe(subscription_settings(
            source.id(),
            "stale-resume-q",
            Some(requested),
            true,
        ))
        .await
    {
        Ok(_) => panic!("expected stale resume_from to fail"),
        Err(err) => err,
    };

    match err.downcast_ref::<SourceError>() {
        Some(SourceError::PositionUnavailable {
            source_id,
            requested: actual_requested,
            earliest_available: Some(actual_earliest),
        }) => {
            assert_eq!(source_id, source.id());
            assert_eq!(*actual_requested, requested);
            assert_eq!(*actual_earliest, earliest);
        }
        other => panic!("expected PositionUnavailable, got {other:?}"),
    }

    source.stop().await?;
    pg.cleanup().await;

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_get_slot_confirmed_flush_lsn_returns_none_when_slot_missing() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    let lsn = get_slot_confirmed_flush_lsn(&client, "missing_slot").await?;
    assert_eq!(lsn, None);

    pg.cleanup().await;

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_start_surfaces_replication_setup_failure() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let mut source_config = build_source_config(pg.config(), slot_name());
    source_config.slot_name = "invalid slot name".to_string();

    let source = PostgresReplicationSource::builder("pg-bad-source")
        .with_config(source_config)
        .build()?;

    let err = match source.start().await {
        Ok(_) => panic!("expected source.start() to surface replication setup failure"),
        Err(err) => err,
    };

    let message = err.to_string();
    assert!(
        message.contains("Failed to establish PostgreSQL replication")
            || message.contains("START_REPLICATION failed")
            || message.contains("publication"),
        "unexpected error when replication setup failed: {message}"
    );

    assert_ne!(
        source.status().await,
        drasi_lib::channels::ComponentStatus::Running,
        "source should not report Running when replication startup fails"
    );

    pg.cleanup().await;

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_decimal_datatype_serialization() -> Result<()> {
    init_logging();

    const DECIMAL_TABLE: &str = "products";
    const DECIMAL_PUBLICATION: &str = "drasi_decimal_pub";

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_decimal_test_table(&client, DECIMAL_TABLE).await?;
    grant_table_access(&client, DECIMAL_TABLE, "postgres").await?;
    create_publication(&client, DECIMAL_PUBLICATION, &[DECIMAL_TABLE.to_string()]).await?;

    let slot_name = slot_name();
    create_logical_replication_slot(&client, &slot_name).await?;

    // Build a DrasiLib instance for the decimal table
    let source_config = PostgresSourceConfig {
        host: pg.config().host.clone(),
        port: pg.config().port,
        database: pg.config().database.clone(),
        user: pg.config().user.clone(),
        password: pg.config().password.clone(),
        tables: vec![DECIMAL_TABLE.to_string()],
        slot_name: slot_name.clone(),
        publication_name: DECIMAL_PUBLICATION.to_string(),
        ssl_mode: SslMode::Disable,
        table_keys: vec![TableKeyConfig {
            table: DECIMAL_TABLE.to_string(),
            key_columns: vec!["id".to_string()],
        }],
    };

    let bootstrap_config = PostgresBootstrapConfig {
        host: source_config.host.clone(),
        port: source_config.port,
        database: source_config.database.clone(),
        user: source_config.user.clone(),
        password: source_config.password.clone(),
        tables: source_config.tables.clone(),
        slot_name: source_config.slot_name.clone(),
        publication_name: source_config.publication_name.clone(),
        ssl_mode: BootstrapSslMode::Disable,
        table_keys: vec![BootstrapTableKeyConfig {
            table: DECIMAL_TABLE.to_string(),
            key_columns: vec!["id".to_string()],
        }],
    };

    let bootstrap_provider = PostgresBootstrapProvider::new(bootstrap_config);

    let source = PostgresReplicationSource::builder("pg-decimal-test-source")
        .with_config(source_config)
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    let query = Query::cypher("decimal-test-query")
        .query(
            r#"
            MATCH (p:products)
            RETURN p.id AS id, p.price AS price, p.quantity AS quantity, p.total AS total
            "#,
        )
        .from_source("pg-decimal-test-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let (reaction, _handle) = ApplicationReaction::builder("decimal-test-reaction")
        .with_query("decimal-test-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("pg-decimal-test-core")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    );

    core.start().await?;
    wait_for_query_results(&core, "decimal-test-query", |results| results.is_empty()).await?;

    // Insert a row with decimal values
    // Using different precisions to test that NUMERIC columns handle various decimal formats
    // price: NUMERIC(10, 2) -> 99.99
    // quantity: NUMERIC(15, 4) -> 10.5000
    // total: NUMERIC(20, 6) -> 1049.895000
    insert_decimal_test_row(&client, DECIMAL_TABLE, 1, "99.99", "10.5000", "1049.895000").await?;

    // Wait for the query results to contain the inserted row
    wait_for_query_results(&core, "decimal-test-query", |results| {
        if results.is_empty() {
            return false;
        }

        let row = &results[0];

        // Verify that decimal values are numbers, not strings
        if let Some(price) = row.get("price") {
            if !price.is_number() {
                log::error!("price is not a number: {price:?}");
                return false;
            }
            // Use approximate comparison for floating point
            if let Some(price_val) = price.as_f64() {
                let expected = 99.99;
                if (price_val - expected).abs() > 0.0001 {
                    log::error!("price value is incorrect: expected {expected}, got {price_val}");
                    return false;
                }
            } else {
                log::error!("price cannot be converted to f64");
                return false;
            }
        } else {
            log::error!("price field is missing");
            return false;
        }

        if let Some(quantity) = row.get("quantity") {
            if !quantity.is_number() {
                log::error!("quantity is not a number: {quantity:?}");
                return false;
            }
            // Use approximate comparison for floating point
            if let Some(quantity_val) = quantity.as_f64() {
                let expected = 10.5;
                if (quantity_val - expected).abs() > 0.0001 {
                    log::error!(
                        "quantity value is incorrect: expected {expected}, got {quantity_val}"
                    );
                    return false;
                }
            } else {
                log::error!("quantity cannot be converted to f64");
                return false;
            }
        } else {
            log::error!("quantity field is missing");
            return false;
        }

        if let Some(total) = row.get("total") {
            if !total.is_number() {
                log::error!("total is not a number: {total:?}");
                return false;
            }
            // Use approximate comparison for floating point
            if let Some(total_val) = total.as_f64() {
                let expected = 1049.895000;
                if (total_val - expected).abs() > 0.0001 {
                    log::error!("total value is incorrect: expected {expected}, got {total_val}");
                    return false;
                }
            } else {
                log::error!("total cannot be converted to f64");
                return false;
            }
        } else {
            log::error!("total field is missing");
            return false;
        }

        true
    })
    .await?;

    core.stop().await?;
    pg.cleanup().await;

    Ok(())
}
