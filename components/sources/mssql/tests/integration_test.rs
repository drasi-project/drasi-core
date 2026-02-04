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
        ("ADD", ResultDiff::Add { data })
        | ("DELETE", ResultDiff::Delete { data })
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
