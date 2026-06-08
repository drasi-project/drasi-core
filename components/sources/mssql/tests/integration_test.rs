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

//! Integration tests for MSSQL source using a real MSSQL container.

mod mssql_helpers;

use anyhow::{Context, Result};
use drasi_bootstrap_mssql::MsSqlBootstrapProvider;
use drasi_lib::channels::ResultDiff;
use drasi_lib::indexes::config::{StorageBackendRef, StorageBackendSpec};
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::subscription::SubscriptionOptions;
use drasi_reaction_application::ApplicationReaction;
use drasi_source_mssql::{MsSqlSource, StartPosition};
use mssql_helpers::{execute_sql, setup_mssql, MssqlConfig};
use serde_json::Value;
use std::time::Duration;
use tokio::time::sleep;

const TEST_DB: &str = "DrasiTest";
const TEST_TABLE: &str = "dbo.Products";
const QUERY_ID: &str = "products-query";
const SOURCE_ID: &str = "mssql-source";

const TYPES_TABLE: &str = "dbo.TypesTest";
const TYPES_QUERY_ID: &str = "types-query";
const TYPES_SOURCE_ID: &str = "mssql-types-source";

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn field_matches(data: &Value, field: &str, expected: &str) -> bool {
    data.get(field)
        .and_then(value_as_string)
        .map(|value| value == expected)
        .unwrap_or(false)
}

fn matches_fields(data: &Value, fields: &[(&str, &str)]) -> bool {
    fields
        .iter()
        .all(|(field, expected)| field_matches(data, field, expected))
}

fn matches_change(entry: &ResultDiff, change_type: &str, fields: &[(&str, &str)]) -> bool {
    match (change_type, entry) {
        ("ADD", ResultDiff::Add { data, .. })
        | ("DELETE", ResultDiff::Delete { data, .. })
        | ("UPDATE", ResultDiff::Update { data, .. }) => matches_fields(data, fields),
        _ => false,
    }
}

fn matches_update(
    entry: &ResultDiff,
    before_fields: &[(&str, &str)],
    after_fields: &[(&str, &str)],
) -> bool {
    match entry {
        ResultDiff::Update { before, after, .. } => {
            matches_fields(before, before_fields) && matches_fields(after, after_fields)
        }
        _ => false,
    }
}

async fn wait_for_change<F>(
    subscription: &mut drasi_reaction_application::subscription::Subscription,
    attempts: usize,
    mut matcher: F,
) -> Result<ResultDiff>
where
    F: FnMut(&ResultDiff) -> bool,
{
    for _ in 0..attempts {
        if let Some(result) = subscription.recv().await {
            for entry in &result.results {
                if matcher(entry) {
                    return Ok(entry.clone());
                }
            }
        }
    }

    anyhow::bail!("Timed out waiting for expected change event");
}

async fn prepare_database(config: &MssqlConfig) -> Result<MssqlConfig> {
    let mut client = config.connect().await?;
    execute_sql(
        &mut client,
        &format!("IF DB_ID('{TEST_DB}') IS NULL CREATE DATABASE [{TEST_DB}]"),
    )
    .await?;
    execute_sql(
        &mut client,
        &format!("ALTER DATABASE [{TEST_DB}] SET ALLOW_SNAPSHOT_ISOLATION ON"),
    )
    .await?;
    drop(client);

    let mut db_config = config.clone();
    db_config.database = TEST_DB.to_string();
    let mut db_client = db_config.connect().await?;

    execute_sql(
        &mut db_client,
        "IF NOT EXISTS (SELECT 1 FROM sys.databases WHERE name = DB_NAME() AND is_cdc_enabled = 1)\n            EXEC sys.sp_cdc_enable_db;",
    )
    .await?;
    execute_sql(
        &mut db_client,
        "IF OBJECT_ID('dbo.Products', 'U') IS NOT NULL DROP TABLE dbo.Products;",
    )
    .await?;
    execute_sql(
        &mut db_client,
        "CREATE TABLE dbo.Products (\n            ProductId INT PRIMARY KEY,\n            Name NVARCHAR(100) NOT NULL,\n            Price DECIMAL(10,2) NOT NULL\n        );",
    )
    .await?;
    execute_sql(
        &mut db_client,
        "IF NOT EXISTS (SELECT 1 FROM sys.tables WHERE name = 'Products' AND is_tracked_by_cdc = 1)\n            EXEC sys.sp_cdc_enable_table @source_schema = 'dbo', @source_name = 'Products', @role_name = NULL;",
    )
    .await?;

    // Wait for CDC to be fully initialized (max LSN must be available).
    // CDC capture job starts asynchronously and the first max LSN may not
    // be available immediately after sp_cdc_enable_table returns.
    for _ in 0..30 {
        let result = db_client
            .query("SELECT sys.fn_cdc_get_max_lsn() AS lsn", &[])
            .await?
            .into_first_result()
            .await?;
        if let Some(row) = result.first() {
            if row.try_get::<&[u8], _>(0).ok().flatten().is_some() {
                break;
            }
        }
        sleep(Duration::from_secs(1)).await;
    }

    Ok(db_config)
}

#[tokio::test]
#[ignore] // Run with: cargo test -p drasi-source-mssql --test integration_test -- --ignored --nocapture
#[cfg(not(target_arch = "aarch64"))]
async fn test_mssql_change_detection_end_to_end() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let result = tokio::time::timeout(Duration::from_secs(300), async {
        let mssql = setup_mssql()
            .await
            .context("Failed to start MSSQL container")?;
        let db_config = prepare_database(mssql.config())
            .await
            .context("Failed to prepare MSSQL database")?;

        let bootstrap_provider = MsSqlBootstrapProvider::builder()
            .with_source_id(SOURCE_ID)
            .with_host(&db_config.host)
            .with_port(db_config.port)
            .with_database(&db_config.database)
            .with_user(&db_config.user)
            .with_password(&db_config.password)
            .with_tables(vec![TEST_TABLE.to_string()])
            .build()
            .context("Failed to build MSSQL bootstrap provider")?;

        let source = MsSqlSource::builder(SOURCE_ID)
            .with_host(&db_config.host)
            .with_port(db_config.port)
            .with_database(&db_config.database)
            .with_user(&db_config.user)
            .with_password(&db_config.password)
            .with_table(TEST_TABLE)
            .with_poll_interval_ms(500)
            .with_start_position(StartPosition::Current)
            .with_trust_server_certificate(true)
            .with_bootstrap_provider(bootstrap_provider)
            .build()
            .context("Failed to build MSSQL source")?;

        let query = Query::cypher(QUERY_ID)
            .query(
                r#"
                MATCH (p:Products)
                RETURN p.ProductId AS id, p.Name AS name, p.Price AS price
            "#,
            )
            .from_source(SOURCE_ID)
            .auto_start(true)
            .enable_bootstrap(true)
            .build();

        let (reaction, handle) = ApplicationReaction::builder("app-reaction")
            .with_query(QUERY_ID)
            .build();

        let core = DrasiLib::builder()
            .with_id("mssql-integration-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .context("Failed to build DrasiLib")?;

        core.start().await.context("Failed to start DrasiLib")?;

        let mut subscription = handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .context("Failed to create subscription")?;

        sleep(Duration::from_secs(2)).await;

        let mut client = db_config.connect().await?;
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (1, 'Widget', 19.99);",
        )
        .await?;

        wait_for_change(&mut subscription, 6, |entry| {
            matches_change(entry, "ADD", &[("id", "1"), ("name", "Widget")])
        })
        .await
        .context("Did not observe INSERT change")?;

        execute_sql(
            &mut client,
            "UPDATE dbo.Products SET Name = 'Widget Updated' WHERE ProductId = 1;",
        )
        .await?;

        wait_for_change(&mut subscription, 6, |entry| {
            matches_update(
                entry,
                &[("id", "1"), ("name", "Widget")],
                &[("id", "1"), ("name", "Widget Updated")],
            )
        })
        .await
        .context("Did not observe UPDATE change")?;

        execute_sql(&mut client, "DELETE FROM dbo.Products WHERE ProductId = 1;").await?;

        wait_for_change(&mut subscription, 6, |entry| {
            matches_change(entry, "DELETE", &[("id", "1"), ("name", "Widget Updated")])
        })
        .await
        .context("Did not observe DELETE change")?;

        core.stop().await.context("Failed to stop DrasiLib")?;
        mssql.cleanup().await;

        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => anyhow::bail!("Integration test timed out after 180 seconds"),
    }

    Ok(())
}

#[tokio::test]
#[ignore] // Run with: cargo test -p drasi-source-mssql --test integration_test -- --ignored --nocapture
#[cfg(not(target_arch = "aarch64"))]
async fn test_mssql_schema_discovery_end_to_end() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let result = tokio::time::timeout(Duration::from_secs(300), async {
        let mssql = setup_mssql()
            .await
            .context("Failed to start MSSQL container")?;
        let db_config = prepare_database(mssql.config())
            .await
            .context("Failed to prepare MSSQL database")?;

        let source = MsSqlSource::builder(SOURCE_ID)
            .with_host(&db_config.host)
            .with_port(db_config.port)
            .with_database(&db_config.database)
            .with_user(&db_config.user)
            .with_password(&db_config.password)
            .with_table(TEST_TABLE)
            .with_poll_interval_ms(500)
            .with_start_position(StartPosition::Current)
            .with_trust_server_certificate(true)
            .build()
            .context("Failed to build MSSQL source")?;

        let core = DrasiLib::builder()
            .with_id("mssql-schema-test")
            .with_source(source)
            .build()
            .await
            .context("Failed to build DrasiLib")?;

        core.start().await.context("Failed to start DrasiLib")?;

        let schema = core
            .get_source_schema(SOURCE_ID)
            .await
            .context("Failed to fetch MSSQL source schema")?
            .expect("MSSQL source should report schema");

        assert!(schema.nodes.iter().any(|node| node.label == "Products"));
        let products = schema
            .nodes
            .iter()
            .find(|node| node.label == "Products")
            .expect("Products schema should exist");
        assert!(products
            .properties
            .iter()
            .any(|property| property.name == "ProductId"));
        assert!(products
            .properties
            .iter()
            .any(|property| property.name == "Name"));
        assert!(products
            .properties
            .iter()
            .any(|property| property.name == "Price"));

        core.stop().await.ok();
        mssql.cleanup().await;

        Ok::<(), anyhow::Error>(())
    })
    .await;

    result??;
    Ok(())
}

async fn prepare_types_database(config: &MssqlConfig) -> Result<MssqlConfig> {
    let mut client = config.connect().await?;
    execute_sql(
        &mut client,
        &format!("IF DB_ID('{TEST_DB}') IS NULL CREATE DATABASE [{TEST_DB}]"),
    )
    .await?;
    execute_sql(
        &mut client,
        &format!("ALTER DATABASE [{TEST_DB}] SET ALLOW_SNAPSHOT_ISOLATION ON"),
    )
    .await?;
    drop(client);

    let mut db_config = config.clone();
    db_config.database = TEST_DB.to_string();
    let mut db_client = db_config.connect().await?;

    execute_sql(
        &mut db_client,
        "IF NOT EXISTS (SELECT 1 FROM sys.databases WHERE name = DB_NAME() AND is_cdc_enabled = 1)\n            EXEC sys.sp_cdc_enable_db;",
    )
    .await?;
    execute_sql(
        &mut db_client,
        "IF OBJECT_ID('dbo.TypesTest', 'U') IS NOT NULL BEGIN \
            IF EXISTS (SELECT 1 FROM sys.tables WHERE name = 'TypesTest' AND is_tracked_by_cdc = 1) \
                EXEC sys.sp_cdc_disable_table @source_schema = 'dbo', @source_name = 'TypesTest', @capture_instance = 'all'; \
            DROP TABLE dbo.TypesTest; \
        END",
    )
    .await?;
    execute_sql(
        &mut db_client,
        "CREATE TABLE dbo.TypesTest (
            Id INT PRIMARY KEY,
            IntVal INT NOT NULL,
            BigIntVal BIGINT NOT NULL,
            SmallIntVal SMALLINT NOT NULL,
            TinyIntVal TINYINT NOT NULL,
            BitVal BIT NOT NULL,
            FloatVal FLOAT NOT NULL,
            RealVal REAL NOT NULL,
            DecimalVal DECIMAL(10,2) NOT NULL,
            VarcharVal VARCHAR(100) NOT NULL,
            NVarcharVal NVARCHAR(100) NOT NULL
        );",
    )
    .await?;
    execute_sql(
        &mut db_client,
        "IF NOT EXISTS (SELECT 1 FROM sys.tables WHERE name = 'TypesTest' AND is_tracked_by_cdc = 1)\n            EXEC sys.sp_cdc_enable_table @source_schema = 'dbo', @source_name = 'TypesTest', @role_name = NULL;",
    )
    .await?;

    Ok(db_config)
}

/// Verify that column types are correctly mapped to ElementValue types.
/// Integers should be integers, floats should be floats, booleans should be
/// booleans, and strings should be strings — not everything coerced to string.
#[tokio::test]
#[ignore]
#[cfg(not(target_arch = "aarch64"))]
async fn test_mssql_column_type_mapping() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let result = tokio::time::timeout(Duration::from_secs(300), async {
        let mssql = setup_mssql()
            .await
            .context("Failed to start MSSQL container")?;
        let db_config = prepare_types_database(mssql.config())
            .await
            .context("Failed to prepare MSSQL types database")?;

        let bootstrap_provider = MsSqlBootstrapProvider::builder()
            .with_source_id(TYPES_SOURCE_ID)
            .with_host(&db_config.host)
            .with_port(db_config.port)
            .with_database(&db_config.database)
            .with_user(&db_config.user)
            .with_password(&db_config.password)
            .with_tables(vec![TYPES_TABLE.to_string()])
            .build()
            .context("Failed to build MSSQL bootstrap provider")?;

        let source = MsSqlSource::builder(TYPES_SOURCE_ID)
            .with_host(&db_config.host)
            .with_port(db_config.port)
            .with_database(&db_config.database)
            .with_user(&db_config.user)
            .with_password(&db_config.password)
            .with_table(TYPES_TABLE)
            .with_poll_interval_ms(500)
            .with_start_position(StartPosition::Current)
            .with_trust_server_certificate(true)
            .with_bootstrap_provider(bootstrap_provider)
            .build()
            .context("Failed to build MSSQL source")?;

        let query = Query::cypher(TYPES_QUERY_ID)
            .query(
                r#"
                MATCH (t:TypesTest)
                RETURN t.Id AS id,
                       t.IntVal AS int_val,
                       t.BigIntVal AS bigint_val,
                       t.SmallIntVal AS smallint_val,
                       t.TinyIntVal AS tinyint_val,
                       t.BitVal AS bit_val,
                       t.FloatVal AS float_val,
                       t.RealVal AS real_val,
                       t.DecimalVal AS decimal_val,
                       t.VarcharVal AS varchar_val,
                       t.NVarcharVal AS nvarchar_val
            "#,
            )
            .from_source(TYPES_SOURCE_ID)
            .auto_start(true)
            .enable_bootstrap(true)
            .build();

        let (reaction, handle) = ApplicationReaction::builder("types-reaction")
            .with_query(TYPES_QUERY_ID)
            .build();

        let core = DrasiLib::builder()
            .with_id("mssql-types-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .context("Failed to build DrasiLib")?;

        core.start().await.context("Failed to start DrasiLib")?;

        let mut subscription = handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .context("Failed to create subscription")?;

        sleep(Duration::from_secs(2)).await;

        let mut client = db_config.connect().await?;
        execute_sql(
            &mut client,
            "INSERT INTO dbo.TypesTest (Id, IntVal, BigIntVal, SmallIntVal, TinyIntVal, BitVal, FloatVal, RealVal, DecimalVal, VarcharVal, NVarcharVal) \
             VALUES (1, 42, 9876543210, 256, 7, 1, 3.15, 2.5, 99.95, 'hello', N'world');",
        )
        .await?;

        let change = wait_for_change(&mut subscription, 10, |entry| {
            matches_change(entry, "ADD", &[("id", "1")])
        })
        .await
        .context("Did not observe INSERT change for types test")?;

        if let ResultDiff::Add { data, .. } = &change {
            // Integers should be JSON numbers, not strings
            assert!(
                data.get("int_val").unwrap().is_number(),
                "INT should be a number, got: {}",
                data.get("int_val").unwrap()
            );
            assert_eq!(data.get("int_val").unwrap().as_i64().unwrap(), 42);

            assert!(
                data.get("bigint_val").unwrap().is_number(),
                "BIGINT should be a number, got: {}",
                data.get("bigint_val").unwrap()
            );
            assert_eq!(
                data.get("bigint_val").unwrap().as_i64().unwrap(),
                9876543210
            );

            assert!(
                data.get("smallint_val").unwrap().is_number(),
                "SMALLINT should be a number, got: {}",
                data.get("smallint_val").unwrap()
            );
            assert_eq!(data.get("smallint_val").unwrap().as_i64().unwrap(), 256);

            assert!(
                data.get("tinyint_val").unwrap().is_number(),
                "TINYINT should be a number, got: {}",
                data.get("tinyint_val").unwrap()
            );
            assert_eq!(data.get("tinyint_val").unwrap().as_i64().unwrap(), 7);

            // Boolean
            assert!(
                data.get("bit_val").unwrap().is_boolean(),
                "BIT should be a boolean, got: {}",
                data.get("bit_val").unwrap()
            );
            assert!(data.get("bit_val").unwrap().as_bool().unwrap());

            // Floats should be JSON numbers
            assert!(
                data.get("float_val").unwrap().is_number(),
                "FLOAT should be a number, got: {}",
                data.get("float_val").unwrap()
            );
            let float_val = data.get("float_val").unwrap().as_f64().unwrap();
            assert!(
                (float_val - 3.15).abs() < 0.001,
                "FLOAT value should be ~3.15, got: {float_val}"
            );

            assert!(
                data.get("real_val").unwrap().is_number(),
                "REAL should be a number, got: {}",
                data.get("real_val").unwrap()
            );
            let real_val = data.get("real_val").unwrap().as_f64().unwrap();
            assert!(
                (real_val - 2.5).abs() < 0.01,
                "REAL value should be ~2.5, got: {real_val}"
            );

            assert!(
                data.get("decimal_val").unwrap().is_number(),
                "DECIMAL should be a number, got: {}",
                data.get("decimal_val").unwrap()
            );
            let decimal_val = data.get("decimal_val").unwrap().as_f64().unwrap();
            assert!(
                (decimal_val - 99.95).abs() < 0.01,
                "DECIMAL value should be ~99.95, got: {decimal_val}"
            );

            // Strings should be JSON strings
            assert!(
                data.get("varchar_val").unwrap().is_string(),
                "VARCHAR should be a string, got: {}",
                data.get("varchar_val").unwrap()
            );
            assert_eq!(
                data.get("varchar_val").unwrap().as_str().unwrap(),
                "hello"
            );

            assert!(
                data.get("nvarchar_val").unwrap().is_string(),
                "NVARCHAR should be a string, got: {}",
                data.get("nvarchar_val").unwrap()
            );
            assert_eq!(
                data.get("nvarchar_val").unwrap().as_str().unwrap(),
                "world"
            );
        } else {
            anyhow::bail!("Expected ADD change");
        }

        core.stop().await.context("Failed to stop DrasiLib")?;
        mssql.cleanup().await;

        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => anyhow::bail!("Integration test timed out after 300 seconds"),
    }

    Ok(())
}

// ============================================================================
// Checkpoint / Recovery Integration Tests
// ============================================================================

/// Full query stop/restart checkpoint recovery test.
///
/// 1. Bootstrap from existing data (snapshot captures LSN).
/// 2. Insert rows via CDC so the checkpoint advances.
/// 3. Stop the query, then restart it within the same DrasiLib.
/// 4. On restart the source receives `resume_from` with the last LSN (verified
///    behaviorally by observing that the query resumes streaming new CDC events
///    without re-bootstrapping old data).
///
/// Uses `stop_query`/`start_query` (same pattern as the E2E checkpoint tests)
/// to avoid RocksDB lock contention from spawned background tasks.
#[tokio::test]
#[ignore]
#[cfg(not(target_arch = "aarch64"))]
async fn test_mssql_checkpoint_recovery_round_trip() -> Result<()> {
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use std::sync::Arc;

    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let tmp_dir = tempfile::TempDir::new()?;
    let result = tokio::time::timeout(Duration::from_secs(300), async {
        let mssql = setup_mssql()
            .await
            .context("Failed to start MSSQL container")?;
        let db_config = prepare_database(mssql.config())
            .await
            .context("Failed to prepare MSSQL database")?;

        // Seed the table so bootstrap has data
        let mut client = db_config.connect().await?;
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (200, 'Bootstrap1', 5.00);",
        )
        .await?;
        // Give CDC time to register the insert
        sleep(Duration::from_secs(3)).await;

        let bp = MsSqlBootstrapProvider::builder()
            .with_source_id(SOURCE_ID)
            .with_host(&db_config.host)
            .with_port(db_config.port)
            .with_database(&db_config.database)
            .with_user(&db_config.user)
            .with_password(&db_config.password)
            .with_tables(vec![TEST_TABLE.to_string()])
            .build()?;

        let source = MsSqlSource::builder(SOURCE_ID)
            .with_host(&db_config.host)
            .with_port(db_config.port)
            .with_database(&db_config.database)
            .with_user(&db_config.user)
            .with_password(&db_config.password)
            .with_table(TEST_TABLE)
            .with_poll_interval_ms(500)
            .with_start_position(StartPosition::Current)
            .with_trust_server_certificate(true)
            .with_bootstrap_provider(bp)
            .build()?;

        let query = Query::cypher(QUERY_ID)
            .query(
                r#"
                MATCH (p:Products)
                RETURN p.ProductId AS id, p.Name AS name, p.Price AS price
            "#,
            )
            .from_source(SOURCE_ID)
            .auto_start(true)
            .enable_bootstrap(true)
            .with_storage_backend(StorageBackendRef::Inline(StorageBackendSpec::RocksDb {
                path: tmp_dir.path().to_string_lossy().to_string(),
                enable_archive: false,
                direct_io: false,
            }))
            .build();
        let (reaction, handle) = ApplicationReaction::builder("app-reaction-rt1")
            .with_query(QUERY_ID)
            .build();
        let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

        let core = DrasiLib::builder()
            .with_id("mssql-recovery-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .with_index_provider(Arc::new(provider))
            .build()
            .await
            .context("Failed to build DrasiLib")?;

        core.start().await.context("Failed to start DrasiLib")?;

        let mut sub = handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .context("Failed to create subscription")?;

        // --- Phase 1: Bootstrap + CDC insert ---

        // We should get an ADD for the bootstrapped row
        wait_for_change(&mut sub, 10, |entry| {
            matches_change(entry, "ADD", &[("id", "200"), ("name", "Bootstrap1")])
        })
        .await
        .context("Did not observe bootstrapped row")?;

        // Insert another row via CDC to advance the checkpoint LSN beyond bootstrap
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (201, 'CDC1', 7.00);",
        )
        .await?;

        wait_for_change(&mut sub, 10, |entry| {
            matches_change(entry, "ADD", &[("id", "201"), ("name", "CDC1")])
        })
        .await
        .context("Did not observe CDC row")?;

        // Let checkpoint persist
        sleep(Duration::from_secs(2)).await;

        // --- Stop and restart the query ---
        core.stop_query(QUERY_ID)
            .await
            .context("Failed to stop query")?;

        // Brief pause to let checkpoint flush
        sleep(Duration::from_millis(500)).await;

        core.start_query(QUERY_ID)
            .await
            .context("Failed to restart query")?;

        // --- Phase 2: Verify recovery ---

        // Insert a new row post-restart to prove the source is streaming from
        // the checkpointed position (not re-bootstrapping from scratch)
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (202, 'PostRestart', 11.00);",
        )
        .await?;

        wait_for_change(&mut sub, 15, |entry| {
            matches_change(entry, "ADD", &[("id", "202"), ("name", "PostRestart")])
        })
        .await
        .context("Did not observe post-restart CDC row — checkpoint recovery may have failed")?;

        core.stop().await.context("Failed to stop DrasiLib")?;
        mssql.cleanup().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => anyhow::bail!("test_mssql_checkpoint_recovery_round_trip timed out"),
    }

    Ok(())
}

/// Verify that after bootstrap + query stop + restart, a new CDC insert is visible
/// without duplicates from the previously-consumed data. This proves that the
/// bootstrap snapshot LSN was persisted as the source_position checkpoint and
/// that the CDC stream used it as the resume point.
///
/// Unlike the round-trip test above, this one inserts data *while the query is
/// stopped*, verifying that the CDC stream picks up changes made during the gap.
#[tokio::test]
#[ignore]
#[cfg(not(target_arch = "aarch64"))]
async fn test_mssql_bootstrap_then_restart_resumes_from_snapshot_lsn() -> Result<()> {
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use std::sync::Arc;

    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let tmp_dir = tempfile::TempDir::new()?;
    let result = tokio::time::timeout(Duration::from_secs(300), async {
        let mssql = setup_mssql()
            .await
            .context("Failed to start MSSQL container")?;
        let db_config = prepare_database(mssql.config())
            .await
            .context("Failed to prepare MSSQL database")?;

        let bp = MsSqlBootstrapProvider::builder()
            .with_source_id(SOURCE_ID)
            .with_host(&db_config.host)
            .with_port(db_config.port)
            .with_database(&db_config.database)
            .with_user(&db_config.user)
            .with_password(&db_config.password)
            .with_tables(vec![TEST_TABLE.to_string()])
            .build()?;

        let source = MsSqlSource::builder(SOURCE_ID)
            .with_host(&db_config.host)
            .with_port(db_config.port)
            .with_database(&db_config.database)
            .with_user(&db_config.user)
            .with_password(&db_config.password)
            .with_table(TEST_TABLE)
            .with_poll_interval_ms(500)
            .with_start_position(StartPosition::Current)
            .with_trust_server_certificate(true)
            .with_bootstrap_provider(bp)
            .build()?;

        let query = Query::cypher(QUERY_ID)
            .query(
                r#"
                MATCH (p:Products)
                RETURN p.ProductId AS id, p.Name AS name, p.Price AS price
            "#,
            )
            .from_source(SOURCE_ID)
            .auto_start(true)
            .enable_bootstrap(true)
            .with_storage_backend(StorageBackendRef::Inline(StorageBackendSpec::RocksDb {
                path: tmp_dir.path().to_string_lossy().to_string(),
                enable_archive: false,
                direct_io: false,
            }))
            .build();
        let (reaction, handle) = ApplicationReaction::builder("app-reaction-bs1")
            .with_query(QUERY_ID)
            .build();
        let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

        let core = DrasiLib::builder()
            .with_id("mssql-bootstrap-resume-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .with_index_provider(Arc::new(provider))
            .build()
            .await
            .context("Failed to build DrasiLib")?;

        core.start()
            .await
            .context("Failed to start DrasiLib")?;

        let mut sub = handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .context("Failed to create subscription")?;

        // --- Phase 1: Bootstrap from empty table ---
        // Wait for bootstrap to complete and checkpoint to persist
        sleep(Duration::from_secs(5)).await;

        // Stop the query so we can insert data during the gap
        core.stop_query(QUERY_ID)
            .await
            .context("Failed to stop query")?;

        // Brief pause to let checkpoint flush
        sleep(Duration::from_millis(500)).await;

        // --- Insert data while query is stopped ---
        let mut client = db_config.connect().await?;
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (300, 'AfterBootstrap', 15.00);",
        )
        .await?;
        // Give CDC time to register
        sleep(Duration::from_secs(3)).await;

        // --- Phase 2: Restart query and verify recovery ---
        core.start_query(QUERY_ID)
            .await
            .context("Failed to restart query")?;

        // The row inserted while stopped should be picked up by the CDC
        // stream resuming from the checkpoint's source_position
        wait_for_change(&mut sub, 15, |entry| {
            matches_change(entry, "ADD", &[("id", "300"), ("name", "AfterBootstrap")])
        })
        .await
        .context(
            "Did not observe the row inserted while stopped — \
             bootstrap snapshot LSN checkpoint may not have been persisted correctly",
        )?;

        core.stop()
            .await
            .context("Failed to stop DrasiLib")?;
        mssql.cleanup().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => {
            anyhow::bail!("test_mssql_bootstrap_then_restart_resumes_from_snapshot_lsn timed out")
        }
    }

    Ok(())
}

/// Full system stop/restart test — verifies that a change made while the entire
/// Drasi system is down (including the source) is picked up after restart.
///
/// This test covers a different scenario from the query-only restart tests above:
/// 1. Bootstrap and process some CDC events so checkpoint advances.
/// 2. Stop ALL components (source + query + reaction) via `core.stop()`.
/// 3. Insert a row while everything is stopped.
/// 4. Restart ALL components via `core.start()`.
/// 5. Verify the row inserted during the outage is delivered to the reaction.
///
/// This specifically exercises the CDC polling loop's checkpoint resume path
/// where the source must re-connect, load its persisted LSN checkpoint, and
/// correctly poll changes that occurred while it was offline.
#[tokio::test]
#[ignore]
#[cfg(not(target_arch = "aarch64"))]
async fn test_mssql_full_restart_picks_up_offline_changes() -> Result<()> {
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use std::sync::Arc;

    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let tmp_dir = tempfile::TempDir::new()?;
    let result = tokio::time::timeout(Duration::from_secs(300), async {
        let mssql = setup_mssql()
            .await
            .context("Failed to start MSSQL container")?;
        let db_config = prepare_database(mssql.config())
            .await
            .context("Failed to prepare MSSQL database")?;

        // Seed the table so bootstrap has data
        let mut client = db_config.connect().await?;
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (400, 'Seed', 1.00);",
        )
        .await?;
        sleep(Duration::from_secs(3)).await;

        let bp = MsSqlBootstrapProvider::builder()
            .with_source_id(SOURCE_ID)
            .with_host(&db_config.host)
            .with_port(db_config.port)
            .with_database(&db_config.database)
            .with_user(&db_config.user)
            .with_password(&db_config.password)
            .with_tables(vec![TEST_TABLE.to_string()])
            .build()?;

        let source = MsSqlSource::builder(SOURCE_ID)
            .with_host(&db_config.host)
            .with_port(db_config.port)
            .with_database(&db_config.database)
            .with_user(&db_config.user)
            .with_password(&db_config.password)
            .with_table(TEST_TABLE)
            .with_poll_interval_ms(500)
            .with_start_position(StartPosition::Current)
            .with_trust_server_certificate(true)
            .with_bootstrap_provider(bp)
            .build()?;

        let query = Query::cypher(QUERY_ID)
            .query(
                r#"
                MATCH (p:Products)
                RETURN p.ProductId AS id, p.Name AS name, p.Price AS price
            "#,
            )
            .from_source(SOURCE_ID)
            .auto_start(true)
            .enable_bootstrap(true)
            .with_storage_backend(StorageBackendRef::Inline(StorageBackendSpec::RocksDb {
                path: tmp_dir.path().to_string_lossy().to_string(),
                enable_archive: false,
                direct_io: false,
            }))
            .build();
        let (reaction, handle) = ApplicationReaction::builder("app-reaction-full-restart")
            .with_query(QUERY_ID)
            .build();
        let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

        let core = DrasiLib::builder()
            .with_id("mssql-full-restart-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .with_index_provider(Arc::new(provider))
            .build()
            .await
            .context("Failed to build DrasiLib")?;

        core.start().await.context("Failed to start DrasiLib")?;

        let mut sub = handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .context("Failed to create subscription")?;

        // --- Phase 1: Let bootstrap finish and process one CDC event ---
        sleep(Duration::from_secs(3)).await;

        // Insert a CDC row to advance the checkpoint beyond bootstrap
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (401, 'PreStop', 2.00);",
        )
        .await?;

        wait_for_change(&mut sub, 15, |entry| {
            matches_change(entry, "ADD", &[("id", "401"), ("name", "PreStop")])
        })
        .await
        .context("Did not observe CDC row before stop")?;

        // Let the checkpoint persist
        sleep(Duration::from_secs(2)).await;

        // --- Phase 2: Full stop (source + query + reaction) ---
        core.stop().await.context("Failed to stop DrasiLib")?;

        sleep(Duration::from_millis(500)).await;

        // --- Insert data while everything is stopped ---
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (402, 'WhileStopped', 3.00);",
        )
        .await?;
        // Give CDC agent time to capture the change
        sleep(Duration::from_secs(3)).await;

        // --- Phase 3: Full restart ---
        core.start().await.context("Failed to restart DrasiLib")?;

        // The original subscription should continue receiving events after
        // restart because the reaction instance (and its broadcast channel)
        // persists in-memory across stop/start cycles.

        // --- Phase 4: Verify the offline change is delivered ---
        wait_for_change(&mut sub, 20, |entry| {
            matches_change(entry, "ADD", &[("id", "402"), ("name", "WhileStopped")])
        })
        .await
        .context(
            "Did not observe the row inserted while stopped — \
             full restart CDC resume may have skipped the offline change",
        )?;

        core.stop().await.context("Failed to stop DrasiLib")?;
        mssql.cleanup().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => {
            anyhow::bail!("test_mssql_full_restart_picks_up_offline_changes timed out")
        }
    }

    Ok(())
}

// ============================================================================
// Multi-query replay filtering test.
//
// Verifies that when two queries have different checkpoints and the source
// rewinds to the earlier checkpoint, per-subscriber position filtering
// prevents the more-advanced query from seeing duplicate events.
// ============================================================================

/// Two queries subscribe to the same source. After both process some CDC events,
/// query2 is stopped and query1 advances further. On full restart the source
/// rewinds to query2's (earlier) checkpoint. Position filtering must suppress the
/// replayed events for query1 (so query1 does not re-emit an already-committed row
/// at a new outbox sequence) while delivering them to query2.
///
/// Note: the ApplicationReaction used here is non-durable and replays its query
/// outbox on restart, so query1's existing 602 entry may be re-delivered at the
/// SAME sequence (at-least-once). The assertion below therefore tolerates a
/// same-sequence replay and only fails on a 602 emitted at a NEW, higher sequence.
#[tokio::test]
#[ignore]
#[cfg(not(target_arch = "aarch64"))]
async fn test_mssql_multi_query_no_duplicate_on_restart() -> Result<()> {
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use std::sync::Arc;

    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let tmp_dir = tempfile::TempDir::new()?;
    let result = tokio::time::timeout(Duration::from_secs(300), async {
        let mssql = setup_mssql()
            .await
            .context("Failed to start MSSQL container")?;
        let db_config = prepare_database(mssql.config())
            .await
            .context("Failed to prepare MSSQL database")?;

        let mut client = db_config.connect().await?;

        // Seed so bootstrap has data
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (600, 'Seed', 1.00);",
        )
        .await?;
        sleep(Duration::from_secs(3)).await;

        let bp = MsSqlBootstrapProvider::builder()
            .with_source_id(SOURCE_ID)
            .with_host(&db_config.host)
            .with_port(db_config.port)
            .with_database(&db_config.database)
            .with_user(&db_config.user)
            .with_password(&db_config.password)
            .with_tables(vec![TEST_TABLE.to_string()])
            .build()?;

        let source = MsSqlSource::builder(SOURCE_ID)
            .with_host(&db_config.host)
            .with_port(db_config.port)
            .with_database(&db_config.database)
            .with_user(&db_config.user)
            .with_password(&db_config.password)
            .with_table(TEST_TABLE)
            .with_poll_interval_ms(500)
            .with_start_position(StartPosition::Current)
            .with_trust_server_certificate(true)
            .with_bootstrap_provider(bp)
            .build()?;

        let q1_id = "query1";
        let q2_id = "query2";

        let q1_dir = tmp_dir.path().join("q1");
        let q2_dir = tmp_dir.path().join("q2");
        std::fs::create_dir_all(&q1_dir)?;
        std::fs::create_dir_all(&q2_dir)?;

        let query1 = Query::cypher(q1_id)
            .query(
                r#"
                MATCH (p:Products)
                RETURN p.ProductId AS id, p.Name AS name, p.Price AS price
            "#,
            )
            .from_source(SOURCE_ID)
            .auto_start(true)
            .enable_bootstrap(true)
            .with_storage_backend(StorageBackendRef::Inline(StorageBackendSpec::RocksDb {
                path: q1_dir.to_string_lossy().to_string(),
                enable_archive: false,
                direct_io: false,
            }))
            .build();

        let query2 = Query::cypher(q2_id)
            .query(
                r#"
                MATCH (p:Products)
                RETURN p.ProductId AS id, p.Name AS name, p.Price AS price
            "#,
            )
            .from_source(SOURCE_ID)
            .auto_start(true)
            .enable_bootstrap(true)
            .with_storage_backend(StorageBackendRef::Inline(StorageBackendSpec::RocksDb {
                path: q2_dir.to_string_lossy().to_string(),
                enable_archive: false,
                direct_io: false,
            }))
            .build();

        let (reaction1, handle1) = ApplicationReaction::builder("mq-reaction1")
            .with_query(q1_id)
            .build();
        let (reaction2, handle2) = ApplicationReaction::builder("mq-reaction2")
            .with_query(q2_id)
            .build();

        let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

        let core = DrasiLib::builder()
            .with_id("mssql-multi-query-test")
            .with_source(source)
            .with_query(query1)
            .with_query(query2)
            .with_reaction(reaction1)
            .with_reaction(reaction2)
            .with_index_provider(Arc::new(provider))
            .build()
            .await
            .context("Failed to build DrasiLib")?;

        core.start().await.context("Failed to start DrasiLib")?;

        let sub_opts = SubscriptionOptions::default().with_timeout(Duration::from_secs(5));
        let mut sub1 = handle1
            .subscribe_with_options(sub_opts.clone())
            .await
            .context("subscribe query1")?;
        let mut sub2 = handle2
            .subscribe_with_options(sub_opts.clone())
            .await
            .context("subscribe query2")?;

        // --- Phase 1: Both queries observe a CDC event ---
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (601, 'Both', 2.00);",
        )
        .await?;

        wait_for_change(&mut sub1, 15, |e| {
            matches_change(e, "ADD", &[("id", "601"), ("name", "Both")])
        })
        .await
        .context("query1 did not see row 601")?;

        wait_for_change(&mut sub2, 15, |e| {
            matches_change(e, "ADD", &[("id", "601"), ("name", "Both")])
        })
        .await
        .context("query2 did not see row 601")?;

        // Let checkpoints persist
        sleep(Duration::from_secs(2)).await;

        // --- Phase 2: Stop query2, advance query1 further ---
        core.stop_query(q2_id)
            .await
            .context("Failed to stop query2")?;
        sleep(Duration::from_millis(500)).await;

        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (602, 'OnlyQ1', 3.00);",
        )
        .await?;

        // Capture the outbox sequence at which query1 emits row 602. A non-durable
        // reaction (ApplicationReaction) replays its query outbox on restart, so this
        // exact entry MAY be re-delivered later at the SAME sequence — that is an
        // acceptable at-least-once re-delivery. What must NOT happen is the source
        // re-dispatching the already-committed row 602 to query1, which would cause a
        // NEW emission at a HIGHER sequence.
        let mut q1_602_seq: Option<u64> = None;
        for _ in 0..15 {
            if let Some(result) = sub1.recv().await {
                if result
                    .results
                    .iter()
                    .any(|e| matches_change(e, "ADD", &[("id", "602"), ("name", "OnlyQ1")]))
                {
                    q1_602_seq = Some(result.sequence);
                    break;
                }
            }
        }
        let q1_602_seq = q1_602_seq.context("query1 did not see row 602")?;

        // Let checkpoint persist for query1
        sleep(Duration::from_secs(2)).await;

        // --- Phase 3: Full stop ---
        core.stop().await.context("Failed to stop")?;
        sleep(Duration::from_millis(500)).await;

        // Insert while everything is stopped
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (603, 'WhileStopped', 4.00);",
        )
        .await?;
        sleep(Duration::from_secs(3)).await;

        // --- Phase 4: Full restart ---
        // Source rewinds to min(query1_cp, query2_cp) = query2's earlier checkpoint.
        // Row 602 is replayed by the source but must be filtered for query1 (already
        // committed), so query1 must not RE-EMIT 602 at a new outbox sequence. The
        // non-durable reaction may still re-deliver query1's existing 602 outbox entry
        // (same sequence) as an at-least-once replay, which is acceptable.
        core.start().await.context("Failed to restart")?;

        // The original subscription stays valid across stop/start since the
        // mpsc channel between ApplicationReaction and handle persists.

        // query2 should now see rows 602 and 603 (it missed 602 while stopped)
        wait_for_change(&mut sub2, 20, |e| {
            matches_change(e, "ADD", &[("id", "602"), ("name", "OnlyQ1")])
        })
        .await
        .context("query2 did not see replayed row 602")?;

        wait_for_change(&mut sub2, 20, |e| {
            matches_change(e, "ADD", &[("id", "603"), ("name", "WhileStopped")])
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
                    if matches_change(entry, "ADD", &[("id", "602")])
                        && result.sequence > q1_602_seq
                    {
                        saw_602_new_emission = true;
                    }
                    if matches_change(entry, "ADD", &[("id", "603"), ("name", "WhileStopped")]) {
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

        core.stop().await.context("Failed to stop DrasiLib")?;
        mssql.cleanup().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => anyhow::bail!("test_mssql_multi_query_no_duplicate_on_restart timed out"),
    }

    Ok(())
}

// ============================================================================
// FFI boundary test — verifies checkpoint/recovery through cdylib plugins.
//
// This is the same stop/insert/restart scenario as the test above, but the
// MSSQL source and bootstrap are loaded as cdylib plugins through the FFI
// vtable boundary — the same path used by drasi-server in production.
//
// This catches bugs where position_handle, bootstrap_result_receiver, or
// supports_replay are not wired through FFI and checkpoint data silently
// fails to persist.
// ============================================================================

/// Locate plugin cdylibs in the workspace target directory.
fn ffi_plugin_dir() -> std::path::PathBuf {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // mssql source is at components/sources/mssql, workspace root is ../../../
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .expect("Cannot find workspace root");
    workspace_root.join("target").join("debug").join("plugins")
}

fn ffi_plugin_filename(crate_name: &str) -> String {
    let lib_name = crate_name.replace('-', "_");
    if cfg!(target_os = "macos") {
        format!("lib{lib_name}.dylib")
    } else {
        format!("lib{lib_name}.so")
    }
}

fn ffi_plugin_exists(crate_name: &str) -> bool {
    ffi_plugin_dir()
        .join(ffi_plugin_filename(crate_name))
        .exists()
}

/// Full system stop/restart test through the FFI boundary — the exact scenario
/// the user reported as broken when testing with drasi-server.
///
/// Prerequisite: build cdylib plugins first:
///   cargo build --lib -p drasi-source-mssql --features dynamic-plugin
///   cargo build --lib -p drasi-bootstrap-mssql --features dynamic-plugin
///   cp target/debug/libdrasi_source_mssql.so target/debug/plugins/
///   cp target/debug/libdrasi_bootstrap_mssql.so target/debug/plugins/
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
#[cfg(not(target_arch = "aarch64"))]
async fn test_ffi_mssql_full_restart_picks_up_offline_changes() -> Result<()> {
    use drasi_host_sdk::{callbacks, loader::load_plugin_from_path};
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use drasi_plugin_sdk::descriptor::{BootstrapPluginDescriptor, SourcePluginDescriptor};
    use std::sync::Arc;

    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    if !ffi_plugin_exists("drasi-source-mssql") || !ffi_plugin_exists("drasi-bootstrap-mssql") {
        panic!(
            "SKIP: cdylib plugins not found in {:?}. Build with:\n  \
             cargo build --lib -p drasi-source-mssql --features dynamic-plugin\n  \
             cargo build --lib -p drasi-bootstrap-mssql --features dynamic-plugin\n  \
             cp target/debug/libdrasi_source_mssql.so target/debug/plugins/\n  \
             cp target/debug/libdrasi_bootstrap_mssql.so target/debug/plugins/",
            ffi_plugin_dir()
        );
    }

    let tmp_dir = tempfile::TempDir::new()?;
    let result = tokio::time::timeout(Duration::from_secs(300), async {
        // --- Setup MSSQL testcontainer ---
        let mssql = setup_mssql()
            .await
            .context("Failed to start MSSQL container")?;
        let db_config = prepare_database(mssql.config())
            .await
            .context("Failed to prepare MSSQL database")?;

        // Seed the table so bootstrap has data
        let mut client = db_config.connect().await?;
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (700, 'Seed', 1.00);",
        )
        .await?;
        sleep(Duration::from_secs(3)).await;

        // --- Load cdylib plugins through FFI ---
        let plugin_dir = ffi_plugin_dir();
        let source_path = plugin_dir.join(ffi_plugin_filename("drasi-source-mssql"));
        let bootstrap_path = plugin_dir.join(ffi_plugin_filename("drasi-bootstrap-mssql"));

        let source_plugin = load_plugin_from_path(
            &source_path,
            std::ptr::null_mut(),
            callbacks::default_log_callback_fn(),
            std::ptr::null_mut(),
            callbacks::default_lifecycle_callback_fn(),
        )
        .context("Failed to load MSSQL source cdylib")?;

        let bootstrap_plugin = load_plugin_from_path(
            &bootstrap_path,
            std::ptr::null_mut(),
            callbacks::default_log_callback_fn(),
            std::ptr::null_mut(),
            callbacks::default_lifecycle_callback_fn(),
        )
        .context("Failed to load MSSQL bootstrap cdylib")?;

        // Create source through FFI with JSON config
        let source_config = serde_json::json!({
            "host": db_config.host,
            "port": db_config.port,
            "database": db_config.database,
            "user": db_config.user,
            "password": db_config.password,
            "tables": [TEST_TABLE],
            "pollIntervalMs": 500,
            "trustServerCertificate": true,
            "startPosition": "current"
        });
        let source = source_plugin.source_plugins[0]
            .create_source(SOURCE_ID, &source_config, true)
            .await
            .context("Failed to create MSSQL source through FFI")?;

        // Verify supports_replay through FFI
        assert!(
            source.supports_replay(),
            "MSSQL source must report supports_replay=true through FFI"
        );

        // Create bootstrap provider through FFI
        let bootstrap_config = serde_json::json!({
            "host": db_config.host,
            "port": db_config.port,
            "database": db_config.database,
            "user": db_config.user,
            "password": db_config.password,
            "tables": [TEST_TABLE],
            "trustServerCertificate": true
        });
        let bootstrap_provider = bootstrap_plugin.bootstrap_plugins[0]
            .create_bootstrap_provider(&bootstrap_config, &source_config)
            .await
            .context("Failed to create MSSQL bootstrap through FFI")?;

        // Set bootstrap provider on source (through FFI vtable)
        source.set_bootstrap_provider(bootstrap_provider).await;

        // Build DrasiLib with persistent RocksDB backend
        let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);
        let query = Query::cypher(QUERY_ID)
            .query(
                r#"
                MATCH (p:Products)
                RETURN p.ProductId AS id, p.Name AS name, p.Price AS price
            "#,
            )
            .from_source(SOURCE_ID)
            .auto_start(true)
            .enable_bootstrap(true)
            .with_storage_backend(StorageBackendRef::Inline(StorageBackendSpec::RocksDb {
                path: tmp_dir.path().to_string_lossy().to_string(),
                enable_archive: false,
                direct_io: false,
            }))
            .build();
        let (reaction, handle) = ApplicationReaction::builder("app-reaction-ffi-restart")
            .with_query(QUERY_ID)
            .build();

        let core = DrasiLib::builder()
            .with_id("mssql-ffi-restart-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .with_index_provider(Arc::new(provider))
            .build()
            .await
            .context("Failed to build DrasiLib with FFI plugins")?;

        // --- Phase 1: Start, bootstrap, process a CDC event ---
        core.start().await.context("Failed to start DrasiLib")?;

        let mut sub = handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .context("Failed to create subscription")?;

        // Wait for bootstrap to complete
        sleep(Duration::from_secs(5)).await;

        // Insert a CDC row to advance checkpoint beyond bootstrap
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (701, 'PreStop', 2.00);",
        )
        .await?;

        wait_for_change(&mut sub, 20, |entry| {
            matches_change(entry, "ADD", &[("id", "701"), ("name", "PreStop")])
        })
        .await
        .context("Did not observe CDC row 701 before stop — FFI event dispatch may be broken")?;

        // Let the checkpoint persist
        sleep(Duration::from_secs(2)).await;

        // --- Phase 2: Full stop ---
        core.stop().await.context("Failed to stop DrasiLib")?;
        sleep(Duration::from_millis(500)).await;

        // --- Phase 3: Insert data while everything is stopped ---
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (702, 'WhileStopped', 3.00);",
        )
        .await?;
        sleep(Duration::from_secs(3)).await;

        // --- Phase 4: Restart ---
        // In production, this is a process kill/restart with a new DrasiLib.
        // Here we use stop()/start() on the same instance, which exercises
        // the same checkpoint read-back path: the query reads its RocksDB
        // checkpoint, passes resume_from to the source, and the source
        // rewinds its CDC position. The RocksDB fix (with_txn_or_db) ensures
        // checkpoint reads work outside an active session transaction.
        core.start().await.context("Failed to restart DrasiLib")?;

        // --- Phase 5: Verify the offline change is delivered ---
        wait_for_change(&mut sub, 30, |entry| {
            matches_change(entry, "ADD", &[("id", "702"), ("name", "WhileStopped")])
        })
        .await
        .context(
            "CRITICAL: Row 702 'WhileStopped' was NOT delivered after restart. \
             Checkpoint recovery through FFI boundary is BROKEN. \
             This is the exact user-reported bug: stop Drasi, make changes, \
             restart → changes not delivered.",
        )?;

        core.stop().await.context("Failed to final stop")?;
        mssql.cleanup().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => {
            anyhow::bail!("test_ffi_mssql_full_restart_picks_up_offline_changes timed out")
        }
    }

    Ok(())
}
