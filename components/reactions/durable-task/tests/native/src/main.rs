// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{bail, Context, Result};
use drasi_bootstrap_postgres::PostgresBootstrapProvider;
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{
    channels::DispatchMode, ComponentStatus, DrasiLib, Query, ReactionCheckpoint,
    StateStoreProvider, StorageBackendRef,
};
use drasi_plugin_sdk::prelude::ReactionPluginDescriptor;
use drasi_reaction_durable_task::descriptor::DurableTaskReactionDescriptor;
use drasi_source_postgres::{PostgresReplicationSource, SslMode};
use drasi_state_store_redb::RedbStateStoreProvider;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
enum Action {
    PauseReaction,
    AwaitChange {
        after: u64,
        #[serde(default)]
        outbox: bool,
    },
    Checkpoint,
    Quit,
}

async fn checkpoint(store: &dyn StateStoreProvider) -> Result<u64> {
    match store.get("dts", "checkpoint:order-issues").await? {
        Some(bytes) => Ok(bincode::deserialize::<ReactionCheckpoint>(&bytes)?.sequence),
        None => Ok(0),
    }
}

async fn ready(drasi: &DrasiLib) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let graph = drasi.get_graph().await;
            if ["orders", "order-issues", "dts"].iter().all(|id| {
                graph
                    .nodes
                    .iter()
                    .any(|node| node.id == *id && node.status == ComponentStatus::Running)
            }) {
                return Ok(());
            }
            if graph
                .nodes
                .iter()
                .any(|node| node.status == ComponentStatus::Error)
            {
                bail!("Drasi component entered Error during startup");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("Drasi startup timed out")?
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let filter = match std::env::var("RUST_LOG") {
        Ok(value) => tracing_subscriber::EnvFilter::try_new(value)?,
        Err(std::env::VarError::NotPresent) => tracing_subscriber::EnvFilter::new("warn"),
        Err(error) => return Err(error.into()),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init()
        .map_err(|error| anyhow::anyhow!("Cannot initialize logging: {error}"))?;
    let state_path =
        PathBuf::from(std::env::var("APP_STATE_DIR").context("APP_STATE_DIR is required")?);
    std::fs::create_dir_all(&state_path)?;
    let state = Arc::new(RedbStateStoreProvider::new(state_path.join("state.redb"))?);
    let config_path =
        std::env::var("DTS_REACTION_CONFIG").context("DTS_REACTION_CONFIG is required")?;
    let config: Value = serde_json::from_slice(&std::fs::read(config_path)?)?;
    let reaction = DurableTaskReactionDescriptor
        .create_reaction("dts", vec!["order-issues".into()], &config, true)
        .await?;
    let host = std::env::var("PGHOST").context("PGHOST is required")?;
    let port: u16 = std::env::var("PGPORT")
        .context("PGPORT is required")?
        .parse()
        .context("PGPORT must be a valid port")?;
    let database = std::env::var("PGDATABASE").context("PGDATABASE is required")?;
    let user = std::env::var("PGUSER").context("PGUSER is required")?;
    let password = std::env::var("PGPASSWORD").context("PGPASSWORD is required")?;
    let slot = std::env::var("PGSLOT").context("PGSLOT is required")?;
    let bootstrap = PostgresBootstrapProvider::builder()
        .with_host(&host)
        .with_port(port)
        .with_database(&database)
        .with_user(&user)
        .with_password(&password)
        .with_tables(vec!["recovery_orders".into()])
        .with_table_key("recovery_orders", vec!["episode_id".into()])
        .build();
    let source = PostgresReplicationSource::builder("orders")
        .with_host(host)
        .with_port(port)
        .with_database(database)
        .with_user(user)
        .with_password(password)
        .with_tables(vec!["recovery_orders".into()])
        .with_slot_name(slot)
        .with_publication_name("drasi_recovery_orders")
        .with_ssl_mode(SslMode::Disable)
        .with_dispatch_mode(DispatchMode::Channel)
        .with_bootstrap_provider(bootstrap)
        .build()?;
    let drasi = DrasiLib::builder()
        .with_id("durable-task-example")
        .with_state_store_provider(state.clone())
        .with_index_provider(
            "rocks",
            Arc::new(RocksDbIndexProvider::new(
                state_path.join("rocks"),
                false,
                false,
            )),
        )
        .with_source(source)
        .with_query(
            Query::cypher("order-issues")
                .query(
                    std::fs::read_to_string(
                        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("query.cypher"),
                    )
                    .context("Cannot read query.cypher")?,
                )
                .from_source("orders")
                .with_storage_backend(StorageBackendRef::Named("rocks".into()))
                .with_outbox_capacity(10_000)
                .enable_bootstrap(true)
                .auto_start(true)
                .build(),
        )
        .with_reaction(reaction)
        .build()
        .await?;
    drasi.start().await?;
    ready(&drasi).await?;
    println!("{}", json!({"ready": true}));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let outcome: Result<()> = async {
        while let Some(line) = lines.next_line().await? {
            let action: Action = serde_json::from_str(&line)?;
            let response = match action {
                Action::Quit => break,
                Action::Checkpoint => json!({"sequence": checkpoint(state.as_ref()).await?}),
                Action::PauseReaction => {
                    drasi.stop_reaction("dts").await?;
                    json!({"paused":true})
                }
                Action::AwaitChange { after, outbox } => {
                    let sequence = tokio::time::timeout(Duration::from_secs(60), async {
                        loop {
                            if let Some(node) = drasi.get_graph().await.nodes.iter()
                                .find(|node| node.status == ComponentStatus::Error) {
                                bail!("Drasi component {} failed; inspect drasi.log", node.id);
                            }
                            let current = if outbox {
                                drasi
                                    .query_manager()
                                    .get_query_instance("order-issues")
                                    .await
                                    .map_err(anyhow::Error::msg)?
                                    .fetch_outbox(after)
                                    .await?
                                    .latest_sequence
                            } else {
                                checkpoint(state.as_ref()).await?
                            };
                            if current > after {
                                return Ok::<_, anyhow::Error>(current);
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    })
                    .await
                    .context("Timed out waiting for the PostgreSQL change to reach the outbox/checkpoint")??;
                    json!({"sequence":sequence,"checkpoint":checkpoint(state.as_ref()).await?})
                }
            };
            println!("{response}");
        }
        Ok(())
    }
    .await;
    drasi.stop().await?;
    outcome
}
