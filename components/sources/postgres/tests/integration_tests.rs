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

use anyhow::{Context, Result};
use bytes::Bytes;
use drasi_bootstrap_postgres::{
    PostgresBootstrapConfig, PostgresBootstrapProvider, SslMode as BootstrapSslMode,
    TableKeyConfig as BootstrapTableKeyConfig,
};
use drasi_core::models::SourceChange;
use drasi_lib::channels::SourceEvent;
use drasi_lib::ComponentStatus;
use drasi_lib::{config::SourceSubscriptionSettings, DrasiLib, Query, Source, SourceError};
use drasi_reaction_application::{
    subscription::SubscriptionOptions, ApplicationReaction, ApplicationReactionHandle,
};
use drasi_source_postgres::{
    PostgresReplicationSource, PostgresSourceConfig, SslMode, TableKeyConfig,
};
use postgres_helpers::{
    create_bool_test_table, create_decimal_test_table, create_logical_replication_slot,
    create_publication, create_test_table, create_test_table_replica_identity_default,
    create_timestamptz_test_table, delete_test_row, grant_replication, grant_table_access,
    insert_bool_test_row, insert_decimal_test_row, insert_test_row, insert_timestamptz_test_row,
    setup_replication_postgres, update_bool_test_row, update_test_row,
};
use serial_test::serial;
use std::collections::{HashMap, HashSet};
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

/// Poll a source until it reaches Running state (or timeout after 10s).
async fn wait_for_source_running(source: &PostgresReplicationSource) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if source.status().await == ComponentStatus::Running {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("Source did not reach Running state within 10s");
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
    let source_config = PostgresSourceConfig {
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
async fn test_schema_discovery_reports_columns_end_to_end() -> Result<()> {
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

    let schema = core
        .get_source_schema("pg-test-source")
        .await?
        .expect("postgres source should report schema");

    assert!(schema.nodes.iter().any(|node| node.label == TEST_TABLE));
    let users = schema
        .nodes
        .iter()
        .find(|node| node.label == TEST_TABLE)
        .expect("users schema should be present");
    assert!(users
        .properties
        .iter()
        .any(|property| property.name == "id"));
    assert!(users
        .properties
        .iter()
        .any(|property| property.name == "name"));

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

/// Extract the trailing integer primary key from an element id of the form
/// `table:pk` (e.g. `users:42` → 42).
fn element_id_int(element_id: &str) -> Option<i64> {
    element_id.rsplit(':').next()?.parse::<i64>().ok()
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_bootstrap_cdc_handover_has_no_overlap_or_gap() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    // Seed rows BEFORE creating the replication slot so they are part of the
    // bootstrap snapshot but precede the slot watermark (and thus must never be
    // replayed by CDC).
    let seed_count = 1_000_i32;
    client
        .execute(
            &format!(
                "INSERT INTO {TEST_TABLE} (id, name) SELECT i, 'Seed ' || i::text FROM generate_series(1, $1) AS i"
            ),
            &[&seed_count],
        )
        .await?;

    let slot = slot_name();
    create_logical_replication_slot(&client, &slot).await?;

    // Build a source WITH a bootstrap provider so subscribing triggers the
    // bootstrap snapshot and the bootstrap-to-CDC boundary handover.
    let source_config = build_source_config(pg.config(), slot.clone());
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
    let source = PostgresReplicationSource::builder("pg-direct-source")
        .with_config(source_config)
        .with_bootstrap_provider(PostgresBootstrapProvider::new(bootstrap_config))
        .build()?;

    source.start().await?;
    wait_for_source_running(&source).await;

    let mut settings = subscription_settings("pg-direct-source", "q-handover", None, true);
    settings.enable_bootstrap = true;
    settings.nodes = HashSet::from([TEST_TABLE.to_string()]);
    let response = source.subscribe(settings).await?;
    let mut bootstrap_rx = response
        .bootstrap_receiver
        .expect("bootstrap_receiver should be present when enable_bootstrap is true");
    let mut cdc_rx = response.receiver;

    // Drain the entire bootstrap snapshot. Bootstrap events stop (channel
    // closes) once the snapshot completes and the boundary is published.
    let mut bootstrap_ids: HashMap<i64, usize> = HashMap::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(30), bootstrap_rx.recv()).await {
            Ok(Some(event)) => {
                if let SourceChange::Insert { element } = &event.change {
                    if let Some(id) = element_id_int(element.get_reference().element_id.as_ref()) {
                        *bootstrap_ids.entry(id).or_default() += 1;
                    }
                }
            }
            Ok(None) => break,
            Err(_) => panic!("Timed out draining bootstrap snapshot"),
        }
    }

    assert_eq!(
        bootstrap_ids.len(),
        seed_count as usize,
        "bootstrap should snapshot every seed row exactly once (got {})",
        bootstrap_ids.len()
    );
    for id in 1..=i64::from(seed_count) {
        assert_eq!(
            bootstrap_ids.get(&id).copied().unwrap_or_default(),
            1,
            "seed row {id} missing or duplicated in bootstrap snapshot"
        );
    }

    // Bootstrap is complete and the boundary is published. Mutations performed
    // now are strictly after the boundary and must each be delivered exactly
    // once by CDC, with no seed rows replayed.
    let concurrent_insert_id = i64::from(seed_count) + 1;
    insert_test_row(
        &client,
        TEST_TABLE,
        concurrent_insert_id as i32,
        "Post Boundary",
    )
    .await?;
    update_test_row(&client, TEST_TABLE, 1, "Seed 1 Updated").await?;
    delete_test_row(&client, TEST_TABLE, 2).await?;

    let mut inserts: HashMap<i64, usize> = HashMap::new();
    let mut updates: HashMap<i64, usize> = HashMap::new();
    let mut deletes: HashMap<i64, usize> = HashMap::new();
    let started = Instant::now();
    let mut idle_since = Instant::now();
    while started.elapsed() < Duration::from_secs(30) {
        match tokio::time::timeout(Duration::from_millis(500), cdc_rx.recv()).await {
            Ok(Ok(wrapper)) => {
                idle_since = Instant::now();
                if let SourceEvent::Change(change) = &wrapper.event {
                    if let Some(id) = element_id_int(change.get_reference().element_id.as_ref()) {
                        match change {
                            SourceChange::Insert { .. } => *inserts.entry(id).or_default() += 1,
                            SourceChange::Update { .. } => *updates.entry(id).or_default() += 1,
                            SourceChange::Delete { .. } => *deletes.entry(id).or_default() += 1,
                            SourceChange::Future { .. } => {}
                        }
                    }
                }
            }
            Ok(Err(_)) => break,
            Err(_) => {
                let total_inserts: usize = inserts.values().sum();
                let done = total_inserts == 1
                    && updates.get(&1) == Some(&1)
                    && deletes.get(&2) == Some(&1);
                if done && idle_since.elapsed() >= Duration::from_secs(2) {
                    break;
                }
            }
        }
    }

    // No gap: every post-boundary change was delivered exactly once. The
    // concurrent insert's decoded key is not asserted here because the
    // Postgres CDC element-id encoding for newly inserted integer keys differs
    // from the bootstrap encoding; the structural counts below are what matter.
    let total_inserts: usize = inserts.values().sum();
    assert_eq!(
        total_inserts, 1,
        "post-boundary insert missing or duplicated in CDC stream: {inserts:?}"
    );
    assert_eq!(
        updates.get(&1),
        Some(&1),
        "post-boundary update missing or duplicated in CDC stream: {updates:?}"
    );
    assert_eq!(
        deletes.get(&2),
        Some(&1),
        "post-boundary delete missing or duplicated in CDC stream: {deletes:?}"
    );

    // No overlap: CDC must not replay any pre-boundary seed row. The only
    // change events permitted are the three post-boundary mutations.
    assert_eq!(
        inserts.len(),
        1,
        "unexpected extra inserts (CDC replayed pre-boundary events): {inserts:?}"
    );
    assert_eq!(
        updates.len(),
        1,
        "unexpected extra updates (CDC replayed pre-boundary events): {updates:?}"
    );
    assert_eq!(
        deletes.len(),
        1,
        "unexpected extra deletes (CDC replayed pre-boundary events): {deletes:?}"
    );

    source.stop().await?;
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

/// End-to-end regression test for timestamptz decoding over logical
/// replication. PostgreSQL's pgoutput text protocol emits the timezone offset
/// in short form (e.g. `+00`) rather than the full `+00:00` ISO 8601 form.
/// Before the decoder fix this caused the CDC change to fail to decode, so the
/// inserted row never reached query results. This test inserts a timestamptz
/// row via CDC and asserts it surfaces with the correct instant.
#[tokio::test]
#[serial]
#[ignore]
async fn test_timestamptz_short_offset_serialization() -> Result<()> {
    init_logging();

    const TS_TABLE: &str = "events";
    const TS_PUBLICATION: &str = "drasi_ts_pub";

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_timestamptz_test_table(&client, TS_TABLE).await?;
    grant_table_access(&client, TS_TABLE, "postgres").await?;
    create_publication(&client, TS_PUBLICATION, &[TS_TABLE.to_string()]).await?;

    let slot_name = slot_name();
    create_logical_replication_slot(&client, &slot_name).await?;

    let source_config = PostgresSourceConfig {
        host: pg.config().host.clone(),
        port: pg.config().port,
        database: pg.config().database.clone(),
        user: pg.config().user.clone(),
        password: pg.config().password.clone(),
        tables: vec![TS_TABLE.to_string()],
        slot_name: slot_name.clone(),
        publication_name: TS_PUBLICATION.to_string(),
        ssl_mode: SslMode::Disable,
        table_keys: vec![TableKeyConfig {
            table: TS_TABLE.to_string(),
            key_columns: vec!["id".to_string()],
        }],
    };

    let source = PostgresReplicationSource::builder("pg-ts-test-source")
        .with_config(source_config)
        .build()?;

    let query = Query::cypher("ts-test-query")
        .query(
            r#"
            MATCH (e:events)
            RETURN e.id AS id, e.created_at AS created_at
            "#,
        )
        .from_source("pg-ts-test-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let (reaction, _handle) = ApplicationReaction::builder("ts-test-reaction")
        .with_query("ts-test-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("pg-ts-test-core")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    );

    core.start().await?;

    // Insert a timestamptz row. The value is stored in UTC; the logical
    // replication walsender (server `timezone` GUC = UTC) emits it in text
    // form with a short-form `+00` offset, which is exactly the case the
    // decoder fix addresses.
    let expected = chrono::DateTime::parse_from_rfc3339("2026-05-13T22:03:45.423627+00:00")
        .expect("static timestamp literal should parse")
        .with_timezone(&chrono::Utc);
    insert_timestamptz_test_row(&client, TS_TABLE, 1, expected).await?;

    wait_for_query_results(&core, "ts-test-query", move |results| {
        if results.is_empty() {
            return false;
        }

        let row = &results[0];
        let created_at = match row.get("created_at").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                log::error!("created_at field missing or not a string: {row:?}");
                return false;
            }
        };

        match chrono::DateTime::parse_from_rfc3339(created_at) {
            Ok(parsed) => {
                if parsed.with_timezone(&chrono::Utc) != expected {
                    log::error!("created_at instant mismatch: expected {expected}, got {parsed}");
                    return false;
                }
                true
            }
            Err(e) => {
                log::error!("created_at is not valid rfc3339 ('{created_at}'): {e}");
                false
            }
        }
    })
    .await?;

    core.stop().await?;
    pg.cleanup().await;

    Ok(())
}

/// Collect query results into an `id -> active` boolean map, skipping any row
/// whose `id`/`active` fields are missing or not the expected JSON type.
fn bool_result_map(results: &[serde_json::Value]) -> HashMap<i64, bool> {
    let mut map = HashMap::new();
    for row in results {
        if let (Some(id), Some(active)) = (
            row.get("id").and_then(serde_json::Value::as_i64),
            row.get("active").and_then(serde_json::Value::as_bool),
        ) {
            map.insert(id, active);
        }
    }
    map
}

/// End-to-end regression test for boolean decoding across BOTH the bootstrap
/// snapshot and the pgoutput CDC streaming paths.
///
/// The streaming decoder previously decoded booleans with `data[0] != 0`.
/// Because pgoutput streams column values in text form, `false` arrives as the
/// ASCII byte `"f"` (0x66, non-zero) and was therefore mis-decoded as `true` —
/// so every streamed boolean surfaced as `true`, `true -> false` flips were
/// never observed, and `WHERE active = false` never matched on streamed rows.
/// The bootstrap path (typed `try_get::<bool>`) was always correct.
///
/// This test asserts both paths agree with the underlying rows:
/// - Bootstrap: seed `true` and `false` rows before start; both must surface.
/// - CDC INSERT: a streamed `false` must decode as `false` (not `true`).
/// - CDC UPDATE: a `true -> false` flip must be observed.
#[tokio::test]
#[serial]
#[ignore]
async fn test_boolean_datatype_bootstrap_and_cdc() -> Result<()> {
    init_logging();

    const BOOL_TABLE: &str = "flags";
    const BOOL_PUBLICATION: &str = "drasi_bool_pub";

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_bool_test_table(&client, BOOL_TABLE).await?;
    grant_table_access(&client, BOOL_TABLE, "postgres").await?;
    create_publication(&client, BOOL_PUBLICATION, &[BOOL_TABLE.to_string()]).await?;

    // Seed rows BEFORE creating the slot / starting the source so they are
    // captured via the bootstrap snapshot path. Include both `true` and
    // `false` so a decoder that always yields `true` would fail the `false`
    // assertion.
    insert_bool_test_row(&client, BOOL_TABLE, 1, true).await?;
    insert_bool_test_row(&client, BOOL_TABLE, 2, false).await?;

    let slot_name = slot_name();
    create_logical_replication_slot(&client, &slot_name).await?;

    let source_config = PostgresSourceConfig {
        host: pg.config().host.clone(),
        port: pg.config().port,
        database: pg.config().database.clone(),
        user: pg.config().user.clone(),
        password: pg.config().password.clone(),
        tables: vec![BOOL_TABLE.to_string()],
        slot_name: slot_name.clone(),
        publication_name: BOOL_PUBLICATION.to_string(),
        ssl_mode: SslMode::Disable,
        table_keys: vec![TableKeyConfig {
            table: BOOL_TABLE.to_string(),
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
            table: BOOL_TABLE.to_string(),
            key_columns: vec!["id".to_string()],
        }],
    };

    let bootstrap_provider = PostgresBootstrapProvider::new(bootstrap_config);

    let source = PostgresReplicationSource::builder("pg-bool-test-source")
        .with_config(source_config)
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    let query = Query::cypher("bool-test-query")
        .query(
            r#"
            MATCH (f:flags)
            RETURN f.id AS id, f.active AS active
            "#,
        )
        .from_source("pg-bool-test-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let (reaction, _handle) = ApplicationReaction::builder("bool-test-reaction")
        .with_query("bool-test-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("pg-bool-test-core")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    );

    core.start().await?;

    // --- Bootstrap path: seeded rows must surface with correct boolean values.
    wait_for_query_results(&core, "bool-test-query", |results| {
        let map = bool_result_map(results);
        map.get(&1) == Some(&true) && map.get(&2) == Some(&false)
    })
    .await
    .context("bootstrap should surface id=1 active=true and id=2 active=false")?;

    // --- CDC INSERT path: a streamed `false` must decode as `false`.
    // Under the old decoder this row would have surfaced as `true`.
    insert_bool_test_row(&client, BOOL_TABLE, 3, false).await?;
    insert_bool_test_row(&client, BOOL_TABLE, 4, true).await?;
    wait_for_query_results(&core, "bool-test-query", |results| {
        let map = bool_result_map(results);
        map.get(&3) == Some(&false) && map.get(&4) == Some(&true)
    })
    .await
    .context("CDC insert should surface id=3 active=false and id=4 active=true")?;

    // --- CDC UPDATE path: a `true -> false` flip must be observed.
    // Under the old decoder the flip to `false` was never seen (still `true`).
    update_bool_test_row(&client, BOOL_TABLE, 1, false).await?;
    wait_for_query_results(&core, "bool-test-query", |results| {
        bool_result_map(results).get(&1) == Some(&false)
    })
    .await
    .context("CDC update should surface the id=1 true->false flip")?;

    core.stop().await?;
    pg.cleanup().await;

    Ok(())
}

// --- Replay / Resume support tests ---

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
    resume_from: Option<Bytes>,
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

/// Encode a Postgres LSN (u64) as 8-byte big-endian Bytes for `resume_from`.
fn lsn_to_bytes(lsn: u64) -> Bytes {
    Bytes::from(lsn.to_be_bytes().to_vec())
}

/// Test that subscribing with a resume position before the slot watermark
/// returns SourceError::PositionUnavailable.
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

    let slot = slot_name();
    create_logical_replication_slot(&client, &slot).await?;

    let source = build_source(pg.config(), slot)?;
    source.start().await?;

    // Wait for source to become running
    wait_for_source_running(&source).await;

    // Try to resume from LSN 1 — which is definitely before the slot's consistent_point
    let settings = subscription_settings(
        "pg-direct-source",
        "q-unavailable",
        Some(lsn_to_bytes(1)),
        false,
    );
    let result = source.subscribe(settings).await;

    assert!(result.is_err(), "Expected PositionUnavailable error");
    let err = match result {
        Err(e) => e,
        Ok(_) => unreachable!("already asserted is_err"),
    };
    let source_err: &SourceError = err.downcast_ref().expect("should be SourceError");
    match source_err {
        SourceError::PositionUnavailable {
            source_id,
            requested,
            earliest_available,
        } => {
            assert_eq!(source_id, "pg-direct-source");
            assert_eq!(requested, &lsn_to_bytes(1));
            assert!(earliest_available.is_some());
        }
    }

    source.stop().await?;
    pg.cleanup().await;

    Ok(())
}

/// Test that the source dispatches events with source_position set to WAL LSN bytes.
/// After subscribing and inserting data, verify the received events carry source_position.
#[tokio::test]
#[serial]
#[ignore]
async fn test_events_carry_source_position_bytes() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let slot = slot_name();
    create_logical_replication_slot(&client, &slot).await?;

    let source = build_source(pg.config(), slot)?;
    source.start().await?;

    // Wait for source to become running
    wait_for_source_running(&source).await;

    // Subscribe without resume (fresh subscriber)
    let settings = subscription_settings("pg-direct-source", "q-position-test", None, true);
    let response = source.subscribe(settings).await?;

    // Insert a row
    insert_test_row(&client, TEST_TABLE, 1, "Alice").await?;

    // Read from the change receiver
    let mut rx = response.receiver;
    let start = Instant::now();
    let timeout = Duration::from_secs(15);
    let mut found_event = false;

    while start.elapsed() < timeout {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Ok(event)) => {
                // Verify source_position is set and is 8 bytes
                assert!(
                    event.source_position.is_some(),
                    "Event should have source_position set"
                );
                let pos = event.source_position.as_ref().expect("already checked");
                assert_eq!(pos.len(), 8, "source_position should be 8 bytes (LSN)");

                // Verify it's a valid LSN (non-zero)
                let lsn = u64::from_be_bytes(pos[..8].try_into().expect("8 bytes"));
                assert!(lsn > 0, "LSN should be non-zero");

                // Verify the event has a sequence number stamped by dispatch_event
                assert!(
                    event.sequence.is_some(),
                    "Event should have a sequence number"
                );

                found_event = true;
                break;
            }
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }

    assert!(found_event, "Should have received at least one event");

    source.stop().await?;
    pg.cleanup().await;

    Ok(())
}

/// Test that the source resumes from a WAL LSN and replays events from that point.
#[tokio::test]
#[serial]
#[ignore]
async fn test_resume_subscription_replays_from_lsn() -> Result<()> {
    init_logging();

    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let slot = slot_name();
    create_logical_replication_slot(&client, &slot).await?;

    let source = build_source(pg.config(), slot)?;
    source.start().await?;

    // Wait for source to become running
    wait_for_source_running(&source).await;

    // Subscribe (first subscriber) to capture the initial LSN
    let settings = subscription_settings("pg-direct-source", "q-first", None, true);
    let response = source.subscribe(settings).await?;
    let mut rx = response.receiver;

    // Insert rows
    insert_test_row(&client, TEST_TABLE, 1, "Alice").await?;
    insert_test_row(&client, TEST_TABLE, 2, "Bob").await?;

    // Collect both events and their source_positions
    let mut positions: Vec<Bytes> = Vec::new();
    let start = Instant::now();
    let timeout = Duration::from_secs(15);
    while positions.len() < 2 && start.elapsed() < timeout {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Ok(event)) => {
                if let Some(pos) = event.source_position.clone() {
                    positions.push(pos);
                }
            }
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
    assert_eq!(positions.len(), 2, "Should receive 2 events with positions");

    // The first position is the LSN of the first event. Try resuming from it.
    // This should replay all events from that LSN onwards (i.e., both events).
    let resume_lsn = positions[0].clone();
    source.stop().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Restart and subscribe with resume_from
    source.start().await?;
    wait_for_source_running(&source).await;

    let settings = subscription_settings("pg-direct-source", "q-resumed", Some(resume_lsn), true);
    let response = source.subscribe(settings).await?;
    let mut rx2 = response.receiver;

    // Should receive at least the events from that LSN onwards
    let mut replay_count = 0;
    let start2 = Instant::now();
    while start2.elapsed() < Duration::from_secs(15) {
        match tokio::time::timeout(Duration::from_millis(500), rx2.recv()).await {
            Ok(Ok(_event)) => {
                replay_count += 1;
                if replay_count >= 2 {
                    break;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }

    assert!(
        replay_count >= 2,
        "Should replay at least 2 events from the resumed LSN, got {replay_count}"
    );

    source.stop().await?;
    pg.cleanup().await;

    Ok(())
}

// ============================================================================
// Multi-query replay filtering test.
//
// Verifies that when two queries have different checkpoints and the source
// rewinds to the earlier checkpoint on restart, per-subscriber position
// filtering prevents the more-advanced query from seeing duplicate events.
// ============================================================================

/// Wait for a specific change event on a reaction subscription.
async fn wait_for_subscription_change<F>(
    sub: &mut drasi_reaction_application::subscription::Subscription,
    attempts: usize,
    mut matcher: F,
) -> Result<drasi_lib::channels::ResultDiff>
where
    F: FnMut(&drasi_lib::channels::ResultDiff) -> bool,
{
    for _ in 0..attempts {
        if let Some(result) = sub.recv().await {
            for entry in &result.results {
                if matcher(entry) {
                    return Ok(entry.clone());
                }
            }
        }
    }
    anyhow::bail!("Timed out waiting for expected change event");
}

fn pg_matches_change(
    entry: &drasi_lib::channels::ResultDiff,
    change_type: &str,
    fields: &[(&str, &str)],
) -> bool {
    let data = match (change_type, entry) {
        ("ADD", drasi_lib::channels::ResultDiff::Add { data, .. })
        | ("DELETE", drasi_lib::channels::ResultDiff::Delete { data, .. })
        | ("UPDATE", drasi_lib::channels::ResultDiff::Update { data, .. }) => data,
        _ => return false,
    };
    fields
        .iter()
        .all(|(field, expected)| match data.get(*field) {
            Some(serde_json::Value::String(s)) => s == expected,
            Some(serde_json::Value::Number(n)) => n.to_string() == *expected,
            _ => false,
        })
}

/// Two queries subscribe to the same Postgres source. After both process WAL
/// events, query2 is stopped and query1 advances further. On full restart the
/// source rewinds to query2's earlier checkpoint. Position filtering suppresses
/// replayed events for query1 (so query1 does not re-emit an already-committed
/// row at a new outbox sequence) while delivering them to query2.
///
/// Note: the ApplicationReaction used here is non-durable and replays its query
/// outbox on restart, so query1's existing 602 entry may be re-delivered at the
/// SAME sequence (at-least-once). The assertion below therefore tolerates a
/// same-sequence replay and only fails on a 602 emitted at a NEW, higher sequence.
#[tokio::test]
#[serial]
#[ignore]
async fn test_postgres_multi_query_no_duplicate_on_restart() -> Result<()> {
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use drasi_lib::indexes::config::StorageBackendRef;
    use drasi_reaction_application::subscription::SubscriptionOptions;

    init_logging();

    let tmp_dir = tempfile::TempDir::new()?;
    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let slot = slot_name();
    create_logical_replication_slot(&client, &slot).await?;

    // Seed so bootstrap has data
    insert_test_row(&client, TEST_TABLE, 600, "Seed").await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let source_config = PostgresSourceConfig {
        host: pg.config().host.clone(),
        port: pg.config().port,
        database: pg.config().database.clone(),
        user: pg.config().user.clone(),
        password: pg.config().password.clone(),
        tables: vec![TEST_TABLE.to_string()],
        slot_name: slot.clone(),
        publication_name: TEST_PUBLICATION.to_string(),
        ssl_mode: SslMode::Disable,
        table_keys: vec![TableKeyConfig {
            table: TEST_TABLE.to_string(),
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
            table: TEST_TABLE.to_string(),
            key_columns: vec!["id".to_string()],
        }],
    };

    let bootstrap_provider = PostgresBootstrapProvider::new(bootstrap_config);

    let source = PostgresReplicationSource::builder("pg-mq-source")
        .with_config(source_config)
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    let q1_id = "mq-query1";
    let q2_id = "mq-query2";
    let q1_dir = tmp_dir.path().join("q1");
    let q2_dir = tmp_dir.path().join("q2");
    std::fs::create_dir_all(&q1_dir)?;
    std::fs::create_dir_all(&q2_dir)?;

    let query1 = Query::cypher(q1_id)
        .query(
            r#"
            MATCH (u:users)
            RETURN u.id AS id, u.name AS name
            "#,
        )
        .from_source("pg-mq-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .with_storage_backend(StorageBackendRef::Named("persistent".to_string()))
        .build();

    let query2 = Query::cypher(q2_id)
        .query(
            r#"
            MATCH (u:users)
            RETURN u.id AS id, u.name AS name
            "#,
        )
        .from_source("pg-mq-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .with_storage_backend(StorageBackendRef::Named("persistent".to_string()))
        .build();

    let (reaction1, handle1) = ApplicationReaction::builder("mq-reaction1")
        .with_query(q1_id)
        .build();
    let (reaction2, handle2) = ApplicationReaction::builder("mq-reaction2")
        .with_query(q2_id)
        .build();

    let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("pg-multi-query-test")
            .with_source(source)
            .with_query(query1)
            .with_query(query2)
            .with_reaction(reaction1)
            .with_reaction(reaction2)
            .with_index_provider("persistent", Arc::new(provider))
            .build()
            .await?,
    );

    core.start().await?;

    let sub_opts = SubscriptionOptions::default().with_timeout(Duration::from_secs(5));
    let mut sub1 = handle1.subscribe_with_options(sub_opts.clone()).await?;
    let mut sub2 = handle2.subscribe_with_options(sub_opts.clone()).await?;

    // --- Phase 1: Both queries observe a WAL event ---
    insert_test_row(&client, TEST_TABLE, 601, "Both").await?;

    wait_for_subscription_change(&mut sub1, 15, |e| {
        pg_matches_change(e, "ADD", &[("id", "601"), ("name", "Both")])
    })
    .await?;

    wait_for_subscription_change(&mut sub2, 15, |e| {
        pg_matches_change(e, "ADD", &[("id", "601"), ("name", "Both")])
    })
    .await?;

    // Let checkpoints persist
    tokio::time::sleep(Duration::from_secs(2)).await;

    // --- Phase 2: Stop query2, advance query1 further ---
    core.stop_query(q2_id).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    insert_test_row(&client, TEST_TABLE, 602, "OnlyQ1").await?;

    // Capture the outbox sequence at which query1 emits row 602. A non-durable
    // reaction (ApplicationReaction) replays its query outbox on restart, so this
    // exact entry MAY be re-delivered later at the SAME sequence — an acceptable
    // at-least-once re-delivery. What must NOT happen is the source re-dispatching
    // the already-committed row 602 to query1, causing a NEW emission at a HIGHER
    // sequence.
    let mut q1_602_seq: Option<u64> = None;
    for _ in 0..15 {
        if let Some(result) = sub1.recv().await {
            if result
                .results
                .iter()
                .any(|e| pg_matches_change(e, "ADD", &[("id", "602"), ("name", "OnlyQ1")]))
            {
                q1_602_seq = Some(result.sequence);
                break;
            }
        }
    }
    let q1_602_seq = q1_602_seq.context("query1 did not see row 602")?;
    // Let query1 checkpoint persist
    tokio::time::sleep(Duration::from_secs(2)).await;

    // --- Phase 3: Full stop ---
    core.stop().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Insert while stopped
    insert_test_row(&client, TEST_TABLE, 603, "WhileStopped").await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // --- Phase 4: Full restart ---
    // Source rewinds to min(query1_cp, query2_cp) = query2's earlier checkpoint.
    // Row 602 is replayed by the source but must be filtered for query1 (already
    // committed), so query1 must not RE-EMIT 602 at a new outbox sequence. The
    // non-durable reaction may still re-deliver query1's existing 602 outbox entry
    // (same sequence) as an at-least-once replay, which is acceptable.
    core.start().await?;

    // Reuse sub2 — the mpsc channel survives stop/start because
    // ApplicationReaction.app_tx (the field) keeps the sender alive.
    // query2 should see rows 602 and 603
    wait_for_subscription_change(&mut sub2, 20, |e| {
        pg_matches_change(e, "ADD", &[("id", "602"), ("name", "OnlyQ1")])
    })
    .await
    .context("query2 did not see replayed row 602")?;

    wait_for_subscription_change(&mut sub2, 20, |e| {
        pg_matches_change(e, "ADD", &[("id", "603"), ("name", "WhileStopped")])
    })
    .await
    .context("query2 did not see row 603")?;

    // query1 must see the new row 603. It MAY see row 602 again as an at-least-once
    // replay of its existing outbox entry (same sequence) — acceptable for a
    // non-durable reaction. It must NOT see 602 at a NEW (higher) outbox sequence,
    // which would mean the source re-delivered an already-committed row to query1
    // and query1 re-emitted it (position-filter failure).
    let mut saw_602_new_emission = false;
    let mut saw_603 = false;
    for _ in 0..20 {
        if let Some(result) = sub1.recv().await {
            for entry in &result.results {
                if pg_matches_change(entry, "ADD", &[("id", "602")]) && result.sequence > q1_602_seq
                {
                    saw_602_new_emission = true;
                }
                if pg_matches_change(entry, "ADD", &[("id", "603"), ("name", "WhileStopped")]) {
                    saw_603 = true;
                }
            }
            if saw_603 {
                break;
            }
        }
    }

    assert!(
        saw_603,
        "query1 should see the new row 603 inserted while stopped"
    );
    assert!(
        !saw_602_new_emission,
        "query1 saw row 602 at a NEW outbox sequence (> {q1_602_seq}) after restart — \
         the source position filter must suppress already-committed rows for query1"
    );

    core.stop().await?;
    pg.cleanup().await;

    Ok(())
}

/// Stop → insert while stopped → restart (same instance).  Verifies:
/// 1. The slot watermark was not advanced past query checkpoints
///    (flush_lsn fix), so PositionUnavailable does NOT occur on restart.
/// 2. Queries resume from their checkpoints and receive the missed events.
///
/// Note: In-process we cannot fully simulate a crash (tokio tasks and
/// RocksDB locks survive `drop()`), so we use stop/start on the same
/// DrasiLib instance.  This still exercises the critical code paths:
/// periodic standby feedback uses the corrected flush_lsn, checkpoint
/// persistence, and replication slot resume from checkpoint position.
#[tokio::test]
#[serial]
#[ignore]
async fn test_postgres_stop_restart_recovers() -> Result<()> {
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use drasi_lib::indexes::config::StorageBackendRef;
    use drasi_reaction_application::subscription::SubscriptionOptions;

    init_logging();

    let tmp_dir = tempfile::TempDir::new()?;
    let pg = setup_replication_postgres().await;
    let client = pg.get_client().await?;

    grant_replication(&client, "postgres").await?;
    create_test_table(&client, TEST_TABLE).await?;
    grant_table_access(&client, TEST_TABLE, "postgres").await?;
    create_publication(&client, TEST_PUBLICATION, &[TEST_TABLE.to_string()]).await?;

    let slot = slot_name();
    create_logical_replication_slot(&client, &slot).await?;

    // Seed data
    insert_test_row(&client, TEST_TABLE, 700, "Seed").await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let q_id = "kr-query";
    let q_dir = tmp_dir.path().join("kr-q");
    std::fs::create_dir_all(&q_dir)?;

    let source_config = PostgresSourceConfig {
        host: pg.config().host.clone(),
        port: pg.config().port,
        database: pg.config().database.clone(),
        user: pg.config().user.clone(),
        password: pg.config().password.clone(),
        tables: vec![TEST_TABLE.to_string()],
        slot_name: slot.clone(),
        publication_name: TEST_PUBLICATION.to_string(),
        ssl_mode: SslMode::Disable,
        table_keys: vec![TableKeyConfig {
            table: TEST_TABLE.to_string(),
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
            table: TEST_TABLE.to_string(),
            key_columns: vec!["id".to_string()],
        }],
    };
    let bootstrap_provider = PostgresBootstrapProvider::new(bootstrap_config);

    let source = PostgresReplicationSource::builder("kr-source")
        .with_config(source_config)
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    let query = Query::cypher(q_id)
        .query(
            r#"
            MATCH (u:users)
            RETURN u.id AS id, u.name AS name
            "#,
        )
        .from_source("kr-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .with_storage_backend(StorageBackendRef::Named("persistent".to_string()))
        .build();

    let (reaction, handle) = ApplicationReaction::builder("kr-reaction")
        .with_query(q_id)
        .build();

    let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("pg-stop-restart")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .with_index_provider("persistent", Arc::new(provider))
            .build()
            .await?,
    );

    // --- Phase 1: First run — bootstrap, process events, let checkpoints persist ---
    core.start().await?;

    let sub_opts = SubscriptionOptions::default().with_timeout(Duration::from_secs(5));
    let mut sub1 = handle.subscribe_with_options(sub_opts.clone()).await?;

    // Insert a row and wait for the query to process it
    insert_test_row(&client, TEST_TABLE, 701, "BeforeStop").await?;

    wait_for_subscription_change(&mut sub1, 15, |e| {
        pg_matches_change(e, "ADD", &[("id", "701"), ("name", "BeforeStop")])
    })
    .await
    .context("query did not see row 701")?;

    // Let checkpoint persist
    tokio::time::sleep(Duration::from_secs(3)).await;

    // --- Phase 2: Stop ---
    // The key invariant: periodic standby feedback sent during Phase 1
    // used the corrected flush_lsn derived from query checkpoints, so the
    // slot watermark has NOT advanced past the query's checkpoint.
    core.stop().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // --- Phase 3: Insert rows while stopped ---
    insert_test_row(&client, TEST_TABLE, 702, "WhileStopped1").await?;
    insert_test_row(&client, TEST_TABLE, 703, "WhileStopped2").await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // --- Phase 4: Restart the same instance ---
    // This should NOT fail with PositionUnavailable.
    // Before the flush_lsn fix, the slot watermark would have been
    // advanced past the query's checkpoint, making restart impossible.
    core.start().await?;

    // Reuse the original subscription — the mpsc channel persists across
    // stop/start since the reaction and handle share the same sender/receiver.
    wait_for_subscription_change(&mut sub1, 20, |e| {
        pg_matches_change(e, "ADD", &[("id", "702"), ("name", "WhileStopped1")])
    })
    .await
    .context("query did not see row 702 after restart")?;

    wait_for_subscription_change(&mut sub1, 20, |e| {
        pg_matches_change(e, "ADD", &[("id", "703"), ("name", "WhileStopped2")])
    })
    .await
    .context("query did not see row 703 after restart")?;

    core.stop().await?;
    pg.cleanup().await;

    Ok(())
}
