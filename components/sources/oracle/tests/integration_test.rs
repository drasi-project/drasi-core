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
use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::subscription::SubscriptionOptions;
use drasi_reaction_application::ApplicationReaction;
use drasi_source_oracle::{OracleSource, StartPosition};
use oracle_helpers::{
    delete_product, insert_product, prepare_oracle_database, setup_oracle, update_product,
};
use serde_json::Value;
use serial_test::serial;
use std::time::Duration;
use tokio::time::sleep;

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
        ("ADD", ResultDiff::Add { data })
        | ("DELETE", ResultDiff::Delete { data })
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

        sleep(Duration::from_secs(3)).await;

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
