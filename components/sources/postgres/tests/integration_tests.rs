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
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_postgres::{
    PostgresReplicationSource, PostgresSourceConfig, SslMode, TableKeyConfig,
};
use postgres_helpers::{
    create_logical_replication_slot, create_publication, create_test_table,
    create_test_table_replica_identity_default, delete_test_row, grant_replication,
    grant_table_access, insert_test_row, setup_replication_postgres, update_test_row,
    create_decimal_test_table, insert_decimal_test_row,
};
use serial_test::serial;
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
        let results = core.get_query_results(query_id).await?;
        if predicate(&results) {
            return Ok(());
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
) -> Result<Arc<DrasiLib>> {
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

    let (reaction, _handle) = ApplicationReaction::builder("test-reaction")
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

    Ok(core)
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

    let core = build_core(pg.config(), slot_name).await?;
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

    let core = build_core(pg.config(), slot_name).await?;
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

    let core = build_core(pg.config(), slot_name).await?;
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

    let core = build_core(pg.config(), slot_name).await?;
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

    let core = build_core(pg.config(), slot_name).await?;
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

    let core = build_core(pg.config(), slot_name).await?;
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
            if price.as_f64() != Some(99.99) {
                log::error!("price value is incorrect: {:?}", price.as_f64());
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
            if quantity.as_f64() != Some(10.5) {
                log::error!("quantity value is incorrect: {:?}", quantity.as_f64());
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
