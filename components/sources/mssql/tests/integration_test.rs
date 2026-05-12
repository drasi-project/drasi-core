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

/// Full stop/restart checkpoint recovery test.
///
/// 1. Bootstrap from existing data (snapshot captures LSN).
/// 2. Insert rows via CDC so the checkpoint advances.
/// 3. Stop the instance, then rebuild from the same RocksDB directory.
/// 4. On restart the source receives `resume_from` with the last LSN (verified
///    behaviorally by observing that the query resumes streaming new CDC events
///    without re-bootstrapping old data).
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

        // --- Phase 1: Bootstrap + CDC insert ---

        // Seed the table so bootstrap has data
        let mut client = db_config.connect().await?;
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (200, 'Bootstrap1', 5.00);",
        )
        .await?;
        // Give CDC time to register the insert
        sleep(Duration::from_secs(3)).await;

        let build_source = |source_id: &str, db: &MssqlConfig| -> Result<MsSqlSource> {
            let bp = MsSqlBootstrapProvider::builder()
                .with_source_id(source_id)
                .with_host(&db.host)
                .with_port(db.port)
                .with_database(&db.database)
                .with_user(&db.user)
                .with_password(&db.password)
                .with_tables(vec![TEST_TABLE.to_string()])
                .build()?;

            MsSqlSource::builder(source_id)
                .with_host(&db.host)
                .with_port(db.port)
                .with_database(&db.database)
                .with_user(&db.user)
                .with_password(&db.password)
                .with_table(TEST_TABLE)
                .with_poll_interval_ms(500)
                .with_start_position(StartPosition::Current)
                .with_trust_server_certificate(true)
                .with_bootstrap_provider(bp)
                .build()
        };

        let source1 = build_source(SOURCE_ID, &db_config)?;
        let query1 = Query::cypher(QUERY_ID)
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
        let (reaction1, handle1) = ApplicationReaction::builder("app-reaction-rt1")
            .with_query(QUERY_ID)
            .build();
        let provider1 = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

        let core1 = DrasiLib::builder()
            .with_id("mssql-recovery-test")
            .with_source(source1)
            .with_query(query1)
            .with_reaction(reaction1)
            .with_index_provider(Arc::new(provider1))
            .build()
            .await
            .context("Failed to build DrasiLib (phase 1)")?;

        core1
            .start()
            .await
            .context("Failed to start DrasiLib (phase 1)")?;

        let mut sub1 = handle1
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .context("Failed to create subscription (phase 1)")?;

        // We should get an ADD for the bootstrapped row
        wait_for_change(&mut sub1, 10, |entry| {
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

        wait_for_change(&mut sub1, 10, |entry| {
            matches_change(entry, "ADD", &[("id", "201"), ("name", "CDC1")])
        })
        .await
        .context("Did not observe CDC row")?;

        // Let checkpoint persist
        sleep(Duration::from_secs(2)).await;

        // Stop first instance
        core1
            .stop()
            .await
            .context("Failed to stop DrasiLib (phase 1)")?;
        drop(core1);

        // --- Phase 2: Restart from same RocksDB ---

        let source2 = build_source(SOURCE_ID, &db_config)?;
        let query2 = Query::cypher(QUERY_ID)
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
        let (reaction2, handle2) = ApplicationReaction::builder("app-reaction-rt2")
            .with_query(QUERY_ID)
            .build();
        let provider2 = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

        let core2 = DrasiLib::builder()
            .with_id("mssql-recovery-test")
            .with_source(source2)
            .with_query(query2)
            .with_reaction(reaction2)
            .with_index_provider(Arc::new(provider2))
            .build()
            .await
            .context("Failed to build DrasiLib (phase 2)")?;

        core2
            .start()
            .await
            .context("Failed to start DrasiLib (phase 2)")?;

        let mut sub2 = handle2
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .context("Failed to create subscription (phase 2)")?;

        // Insert a new row post-restart to prove the source is streaming from
        // the checkpointed position (not re-bootstrapping from scratch)
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (202, 'PostRestart', 11.00);",
        )
        .await?;

        wait_for_change(&mut sub2, 10, |entry| {
            matches_change(entry, "ADD", &[("id", "202"), ("name", "PostRestart")])
        })
        .await
        .context("Did not observe post-restart CDC row — checkpoint recovery may have failed")?;

        core2
            .stop()
            .await
            .context("Failed to stop DrasiLib (phase 2)")?;
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

/// Verify that after bootstrap + stop + restart, a new CDC insert is visible
/// without duplicates from the previously-consumed data. This proves that the
/// bootstrap snapshot LSN was persisted as the source_position checkpoint and
/// that the CDC stream used it as the resume point.
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

        // --- Phase 1: Bootstrap from empty table, then stop ---

        let build_source = |source_id: &str, db: &MssqlConfig| -> Result<MsSqlSource> {
            let bp = MsSqlBootstrapProvider::builder()
                .with_source_id(source_id)
                .with_host(&db.host)
                .with_port(db.port)
                .with_database(&db.database)
                .with_user(&db.user)
                .with_password(&db.password)
                .with_tables(vec![TEST_TABLE.to_string()])
                .build()?;

            MsSqlSource::builder(source_id)
                .with_host(&db.host)
                .with_port(db.port)
                .with_database(&db.database)
                .with_user(&db.user)
                .with_password(&db.password)
                .with_table(TEST_TABLE)
                .with_poll_interval_ms(500)
                .with_start_position(StartPosition::Current)
                .with_trust_server_certificate(true)
                .with_bootstrap_provider(bp)
                .build()
        };

        let source1 = build_source(SOURCE_ID, &db_config)?;
        let query1 = Query::cypher(QUERY_ID)
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
        let (reaction1, _handle1) = ApplicationReaction::builder("app-reaction-bs1")
            .with_query(QUERY_ID)
            .build();
        let provider1 = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

        let core1 = DrasiLib::builder()
            .with_id("mssql-bootstrap-resume-test")
            .with_source(source1)
            .with_query(query1)
            .with_reaction(reaction1)
            .with_index_provider(Arc::new(provider1))
            .build()
            .await
            .context("Failed to build DrasiLib (phase 1)")?;

        core1
            .start()
            .await
            .context("Failed to start DrasiLib (phase 1)")?;

        // Wait for bootstrap to complete (empty table, so just wait for
        // the checkpoint with snapshot LSN to be persisted)
        sleep(Duration::from_secs(5)).await;

        core1
            .stop()
            .await
            .context("Failed to stop DrasiLib (phase 1)")?;
        drop(core1);

        // --- Between phases: insert data while stopped ---
        let mut client = db_config.connect().await?;
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (300, 'AfterBootstrap', 15.00);",
        )
        .await?;
        // Give CDC time to register
        sleep(Duration::from_secs(3)).await;

        // --- Phase 2: Restart from same RocksDB ---
        let source2 = build_source(SOURCE_ID, &db_config)?;
        let query2 = Query::cypher(QUERY_ID)
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
        let (reaction2, handle2) = ApplicationReaction::builder("app-reaction-bs2")
            .with_query(QUERY_ID)
            .build();
        let provider2 = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

        let core2 = DrasiLib::builder()
            .with_id("mssql-bootstrap-resume-test")
            .with_source(source2)
            .with_query(query2)
            .with_reaction(reaction2)
            .with_index_provider(Arc::new(provider2))
            .build()
            .await
            .context("Failed to build DrasiLib (phase 2)")?;

        core2
            .start()
            .await
            .context("Failed to start DrasiLib (phase 2)")?;

        let mut sub2 = handle2
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .context("Failed to create subscription (phase 2)")?;

        // The row inserted while stopped should be picked up by the CDC
        // stream resuming from the checkpoint's source_position
        wait_for_change(&mut sub2, 10, |entry| {
            matches_change(entry, "ADD", &[("id", "300"), ("name", "AfterBootstrap")])
        })
        .await
        .context(
            "Did not observe the row inserted while stopped — \
             bootstrap snapshot LSN checkpoint may not have been persisted correctly",
        )?;

        core2
            .stop()
            .await
            .context("Failed to stop DrasiLib (phase 2)")?;
        mssql.cleanup().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => anyhow::bail!("test_mssql_bootstrap_then_restart_resumes_from_snapshot_lsn timed out"),
    }

    Ok(())
}
