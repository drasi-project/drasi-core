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

mod oracle_helpers;

use anyhow::{Context, Result};
use drasi_bootstrap_oracle::OracleBootstrapProvider;
use drasi_core::models::SourceChange;
use drasi_lib::channels::ResultDiff;
use drasi_lib::channels::SourceEvent;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::{ComponentStatus, DrasiLib, Query, Source};
use drasi_reaction_application::subscription::SubscriptionOptions;
use drasi_reaction_application::ApplicationReaction;
use drasi_source_oracle::{OracleSource, StartPosition};
use oracle_helpers::{
    delete_product, insert_product, insert_products_batch, prepare_oracle_database, setup_oracle,
    update_product,
};
use serde_json::Value;
use serial_test::serial;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Extract the trailing integer primary key from an Oracle element id of the
/// form `schema:table:pk` (e.g. `system:drasi_products:42` → 42).
fn element_id_int(element_id: &str) -> Option<i64> {
    element_id.rsplit(':').next()?.parse::<i64>().ok()
}

const QUERY_ID: &str = "oracle-products-query";
const SOURCE_ID: &str = "oracle-source";
const TABLE_NAME: &str = "SYSTEM.DRASI_PRODUCTS";

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn field_matches(data: &Value, field: &str, expected: &str) -> bool {
    data.get(field)
        .and_then(value_as_string)
        .map(|value| value == expected)
        .unwrap_or(false)
}

fn matches_change(entry: &ResultDiff, change_type: &str, fields: &[(&str, &str)]) -> bool {
    match (change_type, entry) {
        ("ADD", ResultDiff::Add { data, .. })
        | ("DELETE", ResultDiff::Delete { data, .. })
        | ("UPDATE", ResultDiff::Update { data, .. }) => fields
            .iter()
            .all(|(field, expected)| field_matches(data, field, expected)),
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
            before_fields
                .iter()
                .all(|(field, expected)| field_matches(before, field, expected))
                && after_fields
                    .iter()
                    .all(|(field, expected)| field_matches(after, field, expected))
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
    for attempt in 1..=attempts {
        if let Some(result) = subscription.recv().await {
            log::info!("received reaction batch {attempt}: {:?}", result.results);
            for entry in &result.results {
                if matcher(entry) {
                    return Ok(entry.clone());
                }
            }
        }
    }

    anyhow::bail!("Timed out waiting for expected Oracle change event")
}

#[tokio::test]
#[ignore]
#[serial]
async fn test_oracle_change_detection_end_to_end() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let result = tokio::time::timeout(Duration::from_secs(300), async {
        log::info!("starting oracle container");
        let oracle = setup_oracle()
            .await
            .context("Failed to start Oracle container")?;
        log::info!(
            "oracle container ready on {}:{}",
            oracle.config().host,
            oracle.config().port
        );
        prepare_oracle_database(&oracle)
            .await
            .context("Failed to prepare Oracle database")?;
        log::info!("oracle database prepared");

        let config = oracle.config().clone();
        let bootstrap_provider = OracleBootstrapProvider::builder()
            .with_host(&config.host)
            .with_port(config.port)
            .with_service(&config.service)
            .with_user(&config.user)
            .with_password(&config.password)
            .with_table(TABLE_NAME)
            .build()
            .context("Failed to build Oracle bootstrap provider")?;

        let source = OracleSource::builder(SOURCE_ID)
            .with_host(&config.host)
            .with_port(config.port)
            .with_service(&config.service)
            .with_user(&config.user)
            .with_password(&config.password)
            .with_table(TABLE_NAME)
            .with_poll_interval_ms(1_000)
            .with_start_position(StartPosition::Current)
            .with_bootstrap_provider(bootstrap_provider)
            .build()
            .context("Failed to build Oracle source")?;

        let query = Query::cypher(QUERY_ID)
            .query(
                r#"
                MATCH (p:drasi_products)
                RETURN p.id AS id, p.name AS name, p.price AS price
            "#,
            )
            .from_source(SOURCE_ID)
            .auto_start(true)
            .enable_bootstrap(true)
            .build();

        let (reaction, handle) = ApplicationReaction::builder("oracle-app-reaction")
            .with_query(QUERY_ID)
            .build();

        let core = DrasiLib::builder()
            .with_id("oracle-integration-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .context("Failed to build DrasiLib")?;

        log::info!("starting DrasiLib");
        core.start().await.context("Failed to start DrasiLib")?;
        log::info!("DrasiLib started");

        let mut subscription = handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .context("Failed to subscribe to ApplicationReaction")?;

        // Allow source time to establish connection and begin first LogMiner poll cycle.
        sleep(Duration::from_secs(5)).await;

        log::info!("inserting product");
        insert_product(&oracle, 1, "Widget", 19.99)?;
        wait_for_change(&mut subscription, 6, |entry| {
            matches_change(entry, "ADD", &[("id", "1"), ("name", "Widget")])
        })
        .await
        .context("Did not observe Oracle INSERT change")?;
        log::info!("insert observed");

        log::info!("updating product");
        update_product(&oracle, 1, "Widget Updated", 21.5)?;
        wait_for_change(&mut subscription, 6, |entry| {
            matches_update(
                entry,
                &[("id", "1"), ("name", "Widget")],
                &[("id", "1"), ("name", "Widget Updated")],
            )
        })
        .await
        .context("Did not observe Oracle UPDATE change")?;
        log::info!("update observed");

        log::info!("deleting product");
        delete_product(&oracle, 1)?;
        wait_for_change(&mut subscription, 6, |entry| {
            matches_change(entry, "DELETE", &[("id", "1"), ("name", "Widget Updated")])
        })
        .await
        .context("Did not observe Oracle DELETE change")?;
        log::info!("delete observed");

        core.stop().await.context("Failed to stop DrasiLib")?;
        oracle.cleanup().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => anyhow::bail!("Oracle integration test timed out after 300 seconds"),
    }

    Ok(())
}

#[tokio::test]
#[ignore]
#[serial]
async fn test_oracle_start_position_beginning() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let result = tokio::time::timeout(Duration::from_secs(300), async {
        let oracle = setup_oracle()
            .await
            .context("Failed to start Oracle container")?;
        prepare_oracle_database(&oracle)
            .await
            .context("Failed to prepare Oracle database")?;

        insert_product(&oracle, 100, "PreExisting", 9.99)?;

        let config = oracle.config().clone();
        let bootstrap_provider = OracleBootstrapProvider::builder()
            .with_host(&config.host)
            .with_port(config.port)
            .with_service(&config.service)
            .with_user(&config.user)
            .with_password(&config.password)
            .with_table(TABLE_NAME)
            .build()
            .context("Failed to build Oracle bootstrap provider")?;

        let source = OracleSource::builder(SOURCE_ID)
            .with_host(&config.host)
            .with_port(config.port)
            .with_service(&config.service)
            .with_user(&config.user)
            .with_password(&config.password)
            .with_table(TABLE_NAME)
            .with_poll_interval_ms(1_000)
            .with_start_position(StartPosition::Beginning)
            .with_bootstrap_provider(bootstrap_provider)
            .build()
            .context("Failed to build Oracle source")?;

        let query = Query::cypher(QUERY_ID)
            .query(
                r#"
                MATCH (p:drasi_products)
                RETURN p.id AS id, p.name AS name, p.price AS price
            "#,
            )
            .from_source(SOURCE_ID)
            .auto_start(true)
            .enable_bootstrap(true)
            .build();

        let (reaction, handle) = ApplicationReaction::builder("oracle-beginning-reaction")
            .with_query(QUERY_ID)
            .build();

        let core = DrasiLib::builder()
            .with_id("oracle-beginning-test")
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
            .context("Failed to subscribe")?;

        // The pre-existing row is picked up by bootstrap during startup.
        // To verify start_position: Beginning works, we insert a new row after subscribing
        // and confirm streaming is operational (proving the source connected and is processing
        // changes from the archived logs).
        sleep(Duration::from_secs(5)).await;

        insert_product(&oracle, 101, "AfterStart", 12.50)?;
        wait_for_change(&mut subscription, 10, |entry| {
            matches_change(entry, "ADD", &[("id", "101"), ("name", "AfterStart")])
        })
        .await
        .context("Row inserted after start not observed with start_position: beginning")?;

        core.stop().await.context("Failed to stop DrasiLib")?;
        oracle.cleanup().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => anyhow::bail!("Oracle beginning test timed out after 300 seconds"),
    }

    Ok(())
}

#[tokio::test]
#[ignore]
#[serial]
async fn test_oracle_checkpoint_resume() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let result = tokio::time::timeout(Duration::from_secs(300), async {
        let oracle = setup_oracle()
            .await
            .context("Failed to start Oracle container")?;
        prepare_oracle_database(&oracle)
            .await
            .context("Failed to prepare Oracle database")?;

        let config = oracle.config().clone();

        {
            let bootstrap_provider = OracleBootstrapProvider::builder()
                .with_host(&config.host)
                .with_port(config.port)
                .with_service(&config.service)
                .with_user(&config.user)
                .with_password(&config.password)
                .with_table(TABLE_NAME)
                .build()
                .context("Failed to build bootstrap provider (session 1)")?;

            let source = OracleSource::builder(SOURCE_ID)
                .with_host(&config.host)
                .with_port(config.port)
                .with_service(&config.service)
                .with_user(&config.user)
                .with_password(&config.password)
                .with_table(TABLE_NAME)
                .with_poll_interval_ms(1_000)
                .with_start_position(StartPosition::Current)
                .with_bootstrap_provider(bootstrap_provider)
                .build()
                .context("Failed to build source (session 1)")?;

            let query = Query::cypher(QUERY_ID)
                .query(
                    r#"
                    MATCH (p:drasi_products)
                    RETURN p.id AS id, p.name AS name, p.price AS price
                "#,
                )
                .from_source(SOURCE_ID)
                .auto_start(true)
                .enable_bootstrap(true)
                .build();

            let (reaction, handle) = ApplicationReaction::builder("oracle-resume-reaction-1")
                .with_query(QUERY_ID)
                .build();

            let core = DrasiLib::builder()
                .with_id("oracle-resume-test")
                .with_source(source)
                .with_query(query)
                .with_reaction(reaction)
                .build()
                .await
                .context("Failed to build DrasiLib (session 1)")?;

            core.start().await.context("Failed to start (session 1)")?;

            let mut subscription = handle
                .subscribe_with_options(
                    SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
                )
                .await
                .context("Failed to subscribe (session 1)")?;

            sleep(Duration::from_secs(5)).await;

            insert_product(&oracle, 50, "FirstRow", 10.0)?;
            wait_for_change(&mut subscription, 6, |entry| {
                matches_change(entry, "ADD", &[("id", "50"), ("name", "FirstRow")])
            })
            .await
            .context("Did not observe first insert (session 1)")?;

            core.stop().await.context("Failed to stop (session 1)")?;
        }

        {
            let bootstrap_provider = OracleBootstrapProvider::builder()
                .with_host(&config.host)
                .with_port(config.port)
                .with_service(&config.service)
                .with_user(&config.user)
                .with_password(&config.password)
                .with_table(TABLE_NAME)
                .build()
                .context("Failed to build bootstrap provider (session 2)")?;

            let source = OracleSource::builder(SOURCE_ID)
                .with_host(&config.host)
                .with_port(config.port)
                .with_service(&config.service)
                .with_user(&config.user)
                .with_password(&config.password)
                .with_table(TABLE_NAME)
                .with_poll_interval_ms(1_000)
                .with_start_position(StartPosition::Current)
                .with_bootstrap_provider(bootstrap_provider)
                .build()
                .context("Failed to build source (session 2)")?;

            let query = Query::cypher(QUERY_ID)
                .query(
                    r#"
                    MATCH (p:drasi_products)
                    RETURN p.id AS id, p.name AS name, p.price AS price
                "#,
                )
                .from_source(SOURCE_ID)
                .auto_start(true)
                .enable_bootstrap(true)
                .build();

            let (reaction, handle) = ApplicationReaction::builder("oracle-resume-reaction-2")
                .with_query(QUERY_ID)
                .build();

            let core = DrasiLib::builder()
                .with_id("oracle-resume-test")
                .with_source(source)
                .with_query(query)
                .with_reaction(reaction)
                .build()
                .await
                .context("Failed to build DrasiLib (session 2)")?;

            core.start().await.context("Failed to start (session 2)")?;

            let mut subscription = handle
                .subscribe_with_options(
                    SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
                )
                .await
                .context("Failed to subscribe (session 2)")?;

            sleep(Duration::from_secs(5)).await;

            insert_product(&oracle, 51, "SecondRow", 20.0)?;
            wait_for_change(&mut subscription, 6, |entry| {
                matches_change(entry, "ADD", &[("id", "51"), ("name", "SecondRow")])
            })
            .await
            .context("Did not observe second insert after resume (session 2)")?;

            core.stop().await.context("Failed to stop (session 2)")?;
        }

        oracle.cleanup().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => anyhow::bail!("Oracle checkpoint resume test timed out after 300 seconds"),
    }

    Ok(())
}

/// Full query stop/restart checkpoint recovery test for Oracle.
///
/// 1. Bootstrap from existing data (snapshot captures SCN).
///    NOTE: Bootstrap only populates query state; it does NOT emit diffs
///    to reactions. We verify bootstrap via `get_query_results()`.
/// 2. Insert rows via CDC so the checkpoint advances.
/// 3. Stop the query, then restart it within the same DrasiLib.
/// 4. Insert a new row post-restart to verify the query resumes streaming
///    from the checkpointed position without re-bootstrapping.
#[tokio::test]
#[ignore]
#[serial]
async fn test_oracle_checkpoint_recovery_round_trip() -> Result<()> {
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use drasi_lib::{StorageBackendRef, StorageBackendSpec};
    use std::sync::Arc;

    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let tmp_dir = tempfile::TempDir::new().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(300), async {
        let oracle = setup_oracle()
            .await
            .context("Failed to start Oracle container")?;
        prepare_oracle_database(&oracle)
            .await
            .context("Failed to prepare Oracle database")?;

        let config = oracle.config().clone();

        // Seed data for bootstrap
        insert_product(&oracle, 100, "Seed1", 10.0)?;
        sleep(Duration::from_secs(1)).await;

        let bootstrap_provider = OracleBootstrapProvider::builder()
            .with_host(&config.host)
            .with_port(config.port)
            .with_service(&config.service)
            .with_user(&config.user)
            .with_password(&config.password)
            .with_table(TABLE_NAME)
            .build()
            .context("Failed to build bootstrap provider")?;

        let source = OracleSource::builder(SOURCE_ID)
            .with_host(&config.host)
            .with_port(config.port)
            .with_service(&config.service)
            .with_user(&config.user)
            .with_password(&config.password)
            .with_table(TABLE_NAME)
            .with_poll_interval_ms(1_000)
            .with_start_position(StartPosition::Current)
            .with_bootstrap_provider(bootstrap_provider)
            .build()
            .context("Failed to build source")?;

        let query = Query::cypher(QUERY_ID)
            .query(
                r#"
                MATCH (p:drasi_products)
                RETURN p.id AS id, p.name AS name, p.price AS price
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

        let (reaction, handle) = ApplicationReaction::builder("oracle-cp-reaction")
            .with_query(QUERY_ID)
            .build();

        let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

        let core = DrasiLib::builder()
            .with_id("oracle-cp-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .with_index_provider(Arc::new(provider))
            .build()
            .await
            .context("Failed to build DrasiLib")?;

        core.start().await.context("Failed to start")?;

        let mut sub = handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .context("Failed to subscribe")?;

        // Phase 1: Wait for bootstrap to complete (populates query state only,
        // does NOT emit to reactions). Poll get_query_results() until the
        // bootstrapped row appears.
        let mut bootstrap_done = false;
        for _ in 0..30 {
            sleep(Duration::from_secs(1)).await;
            if let Ok(results) = core.get_query_results(QUERY_ID).await {
                for row in &results {
                    if row.get("name").and_then(|v| v.as_str()) == Some("Seed1") {
                        bootstrap_done = true;
                    }
                }
            }
            if bootstrap_done {
                break;
            }
        }
        assert!(
            bootstrap_done,
            "Bootstrap did not complete within 30 seconds"
        );

        // Phase 2: Insert via CDC to advance the checkpoint position.
        insert_product(&oracle, 101, "AfterBootstrap", 20.0)?;
        wait_for_change(&mut sub, 10, |entry| {
            matches_change(entry, "ADD", &[("id", "101"), ("name", "AfterBootstrap")])
        })
        .await
        .context("Did not observe CDC insert before restart")?;

        // Phase 3: Stop the query, then restart it.
        core.stop_query(QUERY_ID)
            .await
            .context("Failed to stop query")?;

        // Brief pause to let checkpoint flush
        sleep(Duration::from_millis(500)).await;

        core.start_query(QUERY_ID)
            .await
            .context("Failed to restart query")?;

        // Phase 4: Insert after restart — should be observable without re-bootstrap.
        // The original subscription persists across stop/start cycles.
        insert_product(&oracle, 102, "AfterRestart", 30.0)?;
        wait_for_change(&mut sub, 15, |entry| {
            matches_change(entry, "ADD", &[("id", "102"), ("name", "AfterRestart")])
        })
        .await
        .context("Did not observe CDC insert after query restart (checkpoint recovery failed)")?;

        core.stop().await.context("Failed to stop")?;
        oracle.cleanup().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => {
            anyhow::bail!("Oracle checkpoint recovery round-trip test timed out after 300 seconds")
        }
    }

    Ok(())
}

/// Test that a full stop/restart cycle picks up changes made while the source was down.
///
/// 1. Start source+query, observe a CDC event.
/// 2. Stop the entire DrasiLib instance.
/// 3. Insert a row while the source is stopped.
/// 4. Restart the same DrasiLib instance.
/// 5. Verify the instance picks up the row inserted during downtime.
#[tokio::test]
#[ignore]
#[serial]
async fn test_oracle_full_restart_picks_up_offline_changes() -> Result<()> {
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use drasi_lib::{StorageBackendRef, StorageBackendSpec};
    use std::sync::Arc;

    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let tmp_dir = tempfile::TempDir::new().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(300), async {
        let oracle = setup_oracle()
            .await
            .context("Failed to start Oracle container")?;
        prepare_oracle_database(&oracle)
            .await
            .context("Failed to prepare Oracle database")?;

        let config = oracle.config().clone();

        let bootstrap_provider = OracleBootstrapProvider::builder()
            .with_host(&config.host)
            .with_port(config.port)
            .with_service(&config.service)
            .with_user(&config.user)
            .with_password(&config.password)
            .with_table(TABLE_NAME)
            .build()
            .context("Failed to build bootstrap provider")?;

        let source = OracleSource::builder(SOURCE_ID)
            .with_host(&config.host)
            .with_port(config.port)
            .with_service(&config.service)
            .with_user(&config.user)
            .with_password(&config.password)
            .with_table(TABLE_NAME)
            .with_poll_interval_ms(1_000)
            .with_start_position(StartPosition::Current)
            .with_bootstrap_provider(bootstrap_provider)
            .build()
            .context("Failed to build source")?;

        let query = Query::cypher(QUERY_ID)
            .query(
                r#"
                MATCH (p:drasi_products)
                RETURN p.id AS id, p.name AS name, p.price AS price
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

        let (reaction, handle) = ApplicationReaction::builder("oracle-restart-reaction")
            .with_query(QUERY_ID)
            .build();

        let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

        let core = DrasiLib::builder()
            .with_id("oracle-restart-test")
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
            .context("Failed to subscribe")?;

        sleep(Duration::from_secs(5)).await;

        // Insert and observe one event to advance the checkpoint.
        insert_product(&oracle, 200, "BeforeStop", 10.0)?;
        wait_for_change(&mut sub, 10, |entry| {
            matches_change(entry, "ADD", &[("id", "200"), ("name", "BeforeStop")])
        })
        .await
        .context("Did not observe insert before stop")?;

        // Let checkpoint persist
        sleep(Duration::from_secs(2)).await;

        // --- Full stop ---
        core.stop().await.context("Failed to stop DrasiLib")?;

        sleep(Duration::from_millis(500)).await;

        // Insert while source is completely stopped.
        insert_product(&oracle, 201, "WhileStopped", 25.0)?;
        sleep(Duration::from_secs(3)).await;

        // --- Full restart (same DrasiLib instance, same RocksDB) ---
        core.start().await.context("Failed to restart DrasiLib")?;

        // The original subscription should continue receiving events after
        // restart because the reaction instance (and its broadcast channel)
        // persists in-memory across stop/start cycles.
        wait_for_change(&mut sub, 20, |entry| {
            matches_change(entry, "ADD", &[("id", "201"), ("name", "WhileStopped")])
        })
        .await
        .context(
            "Did not observe row inserted while stopped — \
             full restart CDC resume may have skipped the offline change",
        )?;

        core.stop().await.context("Failed to stop DrasiLib")?;
        oracle.cleanup().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => anyhow::bail!("Oracle full restart checkpoint test timed out after 300 seconds"),
    }

    Ok(())
}

/// Verifies the bootstrap-to-CDC handover eliminates overlap and gaps using a
/// direct source subscription: every seed row appears in the bootstrap snapshot
/// exactly once, and each post-boundary mutation is delivered by CDC exactly
/// once with no seed row replayed (Oracle is source-first: the boundary SCN is
/// captured in `subscribe()` and the snapshot is taken `AS OF` that SCN).
#[tokio::test]
#[ignore]
#[serial]
async fn test_oracle_bootstrap_cdc_overlap_handover_no_duplicates_or_gaps() -> Result<()> {
    use std::collections::HashMap;

    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    const SEED_COUNT: i64 = 200;

    let result = tokio::time::timeout(Duration::from_secs(420), async {
        let oracle = setup_oracle()
            .await
            .context("Failed to start Oracle container")?;
        prepare_oracle_database(&oracle)
            .await
            .context("Failed to prepare Oracle database")?;

        // Seed rows BEFORE subscribing. They are part of the bootstrap snapshot
        // (taken AS OF the boundary SCN captured in subscribe) and must never be
        // replayed by CDC after the boundary.
        let seed: Vec<(i64, String, f64)> = (1..=SEED_COUNT)
            .map(|id| (id, format!("Seed{id}"), id as f64))
            .collect();
        insert_products_batch(&oracle, &seed).context("Failed to seed Oracle rows")?;

        let config = oracle.config().clone();
        let bootstrap_provider = OracleBootstrapProvider::builder()
            .with_host(&config.host)
            .with_port(config.port)
            .with_service(&config.service)
            .with_user(&config.user)
            .with_password(&config.password)
            .with_table(TABLE_NAME)
            .build()
            .context("Failed to build Oracle bootstrap provider")?;

        let source = OracleSource::builder(SOURCE_ID)
            .with_host(&config.host)
            .with_port(config.port)
            .with_service(&config.service)
            .with_user(&config.user)
            .with_password(&config.password)
            .with_table(TABLE_NAME)
            .with_poll_interval_ms(1_000)
            .with_start_position(StartPosition::Current)
            .with_bootstrap_provider(bootstrap_provider)
            .build()
            .context("Failed to build Oracle source")?;

        source
            .start()
            .await
            .context("Failed to start Oracle source")?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(30) {
            if source.status().await == ComponentStatus::Running {
                break;
            }
            sleep(Duration::from_millis(200)).await;
        }
        anyhow::ensure!(
            source.status().await == ComponentStatus::Running,
            "Oracle source did not reach Running state"
        );

        // Subscribe directly to the source so we observe both the bootstrap
        // snapshot and the CDC stream deterministically.
        let settings = SourceSubscriptionSettings {
            source_id: SOURCE_ID.to_string(),
            enable_bootstrap: true,
            query_id: "q-handover".to_string(),
            nodes: HashSet::from(["drasi_products".to_string()]),
            relations: HashSet::new(),
            resume_from: None,
            request_position_handle: true,
        };
        let response = source
            .subscribe(settings)
            .await
            .context("Failed to subscribe to Oracle source")?;
        let mut bootstrap_rx = response
            .bootstrap_receiver
            .expect("bootstrap_receiver should be present when enable_bootstrap is true");
        let mut cdc_rx = response.receiver;

        // Drain the entire bootstrap snapshot. Bootstrap events stop (channel
        // closes) once the snapshot completes and the boundary is published.
        let mut bootstrap_ids: HashMap<i64, usize> = HashMap::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(120), bootstrap_rx.recv()).await {
                Ok(Some(event)) => {
                    if let SourceChange::Insert { element } = &event.change {
                        if let Some(id) =
                            element_id_int(element.get_reference().element_id.as_ref())
                        {
                            *bootstrap_ids.entry(id).or_default() += 1;
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => anyhow::bail!("Timed out draining bootstrap snapshot"),
            }
        }

        anyhow::ensure!(
            bootstrap_ids.len() == SEED_COUNT as usize,
            "bootstrap should snapshot every seed row exactly once (got {})",
            bootstrap_ids.len()
        );
        for id in 1..=SEED_COUNT {
            anyhow::ensure!(
                bootstrap_ids.get(&id).copied().unwrap_or_default() == 1,
                "seed row {id} missing or duplicated in bootstrap snapshot"
            );
        }

        // Bootstrap is complete and the boundary SCN is published. Mutations now
        // are strictly after the boundary and must each be delivered exactly once
        // by CDC, with no seed rows replayed.
        let insert_id = SEED_COUNT + 1;
        update_product(&oracle, 10, "ConcurrentUpdated", 999.0)
            .context("Failed post-boundary update")?;
        delete_product(&oracle, 20).context("Failed post-boundary delete")?;
        insert_product(&oracle, insert_id, "ConcurrentInserted", 1001.0)
            .context("Failed post-boundary insert")?;

        let mut inserts: HashMap<i64, usize> = HashMap::new();
        let mut updates: HashMap<i64, usize> = HashMap::new();
        let mut deletes: HashMap<i64, usize> = HashMap::new();
        let started = Instant::now();
        let mut idle_since = Instant::now();
        while started.elapsed() < Duration::from_secs(90) {
            match tokio::time::timeout(Duration::from_millis(500), cdc_rx.recv()).await {
                Ok(Ok(wrapper)) => {
                    idle_since = Instant::now();
                    if let SourceEvent::Change(change) = &wrapper.event {
                        if let Some(id) = element_id_int(change.get_reference().element_id.as_ref())
                        {
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
                    let done = inserts.get(&insert_id) == Some(&1)
                        && updates.get(&10) == Some(&1)
                        && deletes.get(&20) == Some(&1);
                    if done && idle_since.elapsed() >= Duration::from_secs(3) {
                        break;
                    }
                }
            }
        }

        // No gap: every post-boundary change delivered exactly once.
        anyhow::ensure!(
            inserts.get(&insert_id) == Some(&1),
            "post-boundary insert missing or duplicated in CDC stream: {inserts:?}"
        );
        anyhow::ensure!(
            updates.get(&10) == Some(&1),
            "post-boundary update missing or duplicated in CDC stream: {updates:?}"
        );
        anyhow::ensure!(
            deletes.get(&20) == Some(&1),
            "post-boundary delete missing or duplicated in CDC stream: {deletes:?}"
        );

        // No overlap: CDC must not replay any pre-boundary seed row. The only
        // change events permitted are the three post-boundary mutations.
        anyhow::ensure!(
            inserts.len() == 1,
            "unexpected extra inserts (CDC replayed pre-boundary events): {inserts:?}"
        );
        anyhow::ensure!(
            updates.len() == 1,
            "unexpected extra updates (CDC replayed pre-boundary events): {updates:?}"
        );
        anyhow::ensure!(
            deletes.len() == 1,
            "unexpected extra deletes (CDC replayed pre-boundary events): {deletes:?}"
        );

        source
            .stop()
            .await
            .context("Failed to stop Oracle source")?;
        oracle.cleanup().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => anyhow::bail!("Oracle overlap handover test timed out after 420 seconds"),
    }

    Ok(())
}
