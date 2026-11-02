// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

mod service;

use anyhow::{ensure, Context, Result};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{DrasiLib, Query, ReactionCheckpoint, StateStoreProvider};
use drasi_plugin_sdk::ReactionPluginDescriptor;
use drasi_reaction_temporal::descriptor::TemporalReactionDescriptor;
use drasi_source_postgres::{PostgresReplicationSource, SslMode};
use drasi_state_store_redb::RedbStateStoreProvider;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};

const QUERY_IDS: [&str; 4] = ["placed", "paid", "shipped", "delivered"];

async fn status(core: &DrasiLib, store: &impl StateStoreProvider) -> Result<Value> {
    let mut queries = serde_json::Map::new();
    for id in QUERY_IDS {
        let checkpoint = store
            .get("actions", &format!("checkpoint:{id}"))
            .await?
            .map(|v| bincode::deserialize::<ReactionCheckpoint>(&v))
            .transpose()?;
        queries.insert(
            id.into(),
            json!({
                "status": format!("{:?}", core.get_query_status(id).await?),
                "checkpoint": checkpoint,
                "results": core.get_query_results(id).await?
            }),
        );
    }
    Ok(json!({
        "status": format!("{:?}", core.get_reaction_status("actions").await?),
        "sourceStatus": format!("{:?}", core.get_source_status("input").await?),
        "queries": queries
    }))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--healthcheck"] {
        return service::healthcheck().await;
    }
    ensure!(args.is_empty(), "expected --healthcheck or no arguments");
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .map_err(|error| anyhow::anyhow!("initialize tracing: {error}"))?;
    let dir = PathBuf::from(std::env::var("DATA_DIR").context("DATA_DIR is required")?);
    std::fs::create_dir_all(&dir)?;
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(
        std::env::var("TEMPORAL_CONFIG_FILE").context("TEMPORAL_CONFIG_FILE is required")?,
    )?)?;
    let reaction = TemporalReactionDescriptor
        .create_reaction(
            "actions",
            QUERY_IDS.iter().map(|id| (*id).into()).collect(),
            &raw,
            true,
        )
        .await?;
    let store = Arc::new(RedbStateStoreProvider::new(dir.join("reaction.redb"))?);
    let source = PostgresReplicationSource::builder("input")
        .with_host(std::env::var("PGHOST").context("PGHOST is required")?)
        .with_port(
            std::env::var("PGPORT")
                .context("PGPORT is required")?
                .parse()?,
        )
        .with_database(std::env::var("PGDATABASE").context("PGDATABASE is required")?)
        .with_user(std::env::var("PGUSER").context("PGUSER is required")?)
        .with_password(std::env::var("PGPASSWORD").context("PGPASSWORD is required")?)
        .with_slot_name(std::env::var("PGSLOT").context("PGSLOT is required")?)
        .with_publication_name("drasi_temporal")
        .with_tables(vec![
            std::env::var("PGTABLE").context("PGTABLE is required")?
        ])
        .with_ssl_mode(SslMode::Disable)
        .build()?;
    let query_dir = PathBuf::from(std::env::var("QUERY_DIR").context("QUERY_DIR is required")?);
    let mut builder = DrasiLib::builder()
        .with_id("temporal-actions-example")
        .with_source(source)
        .with_reaction(reaction)
        .with_default_index_provider(
            "rocks",
            Arc::new(RocksDbIndexProvider::new(dir.join("index"), false, false)),
        )
        .with_state_store_provider(store.clone());
    for id in QUERY_IDS {
        let query_text = std::fs::read_to_string(query_dir.join(format!("{id}.cypher")))?;
        let query = Query::cypher(id)
            .query(query_text)
            .from_source("input")
            .enable_bootstrap(false)
            .auto_start(true)
            .build();
        builder = builder.with_query(query);
    }
    let core = builder.build().await?;
    core.start().await?;
    ensure!(
        core.get_source_status("input").await? == drasi_lib::ComponentStatus::Running,
        "PostgreSQL source did not start"
    );
    ensure!(
        core.get_reaction_status("actions").await? == drasi_lib::ComponentStatus::Running,
        "reaction did not start"
    );
    let outcome = service::run(&core, store.as_ref()).await;
    core.stop().await?;
    outcome
}
