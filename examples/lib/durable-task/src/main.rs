// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{bail, Context, Result};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{channels::DispatchMode, ComponentStatus, DrasiLib, Query, StorageBackendRef};
use drasi_plugin_sdk::prelude::ReactionPluginDescriptor;
use drasi_reaction_durable_task::descriptor::DurableTaskReactionDescriptor;
use drasi_source_postgres::{PostgresReplicationSource, SslMode};
use drasi_state_store_redb::RedbStateStoreProvider;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

async fn ready(drasi: &DrasiLib) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let graph = drasi.get_graph().await;
            if ["orders", "order-status", "dts"].iter().all(|id| {
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
    tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init()
        .map_err(|error| anyhow::anyhow!("Cannot initialize logging: {error}"))?;

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let state_path =
        PathBuf::from(std::env::var("APP_STATE_DIR").context("APP_STATE_DIR is required")?);
    std::fs::create_dir_all(&state_path)?;
    let config: Value = serde_json::from_slice(&std::fs::read(root.join("reaction.json"))?)?;
    let reaction = DurableTaskReactionDescriptor
        .create_reaction("dts", vec!["order-status".into()], &config, true)
        .await?;
    let source = PostgresReplicationSource::builder("orders")
        .with_host(std::env::var("PGHOST").context("PGHOST is required")?)
        .with_port(
            std::env::var("PGPORT")
                .context("PGPORT is required")?
                .parse::<u16>()
                .context("PGPORT must be a valid port")?,
        )
        .with_database(std::env::var("PGDATABASE").context("PGDATABASE is required")?)
        .with_user(std::env::var("PGUSER").context("PGUSER is required")?)
        .with_password(std::env::var("PGPASSWORD").context("PGPASSWORD is required")?)
        .with_tables(vec!["orders".into()])
        .with_slot_name("drasi_orders")
        .with_publication_name("drasi_orders")
        .with_ssl_mode(SslMode::Disable)
        .with_dispatch_mode(DispatchMode::Channel)
        .build()?;
    let drasi = DrasiLib::builder()
        .with_id("durable-task-example")
        .with_state_store_provider(Arc::new(RedbStateStoreProvider::new(
            state_path.join("state.redb"),
        )?))
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
            Query::cypher("order-status")
                .query(std::fs::read_to_string(root.join("query.cypher"))?)
                .from_source("orders")
                .with_storage_backend(StorageBackendRef::Named("rocks".into()))
                .enable_bootstrap(false)
                .build(),
        )
        .with_reaction(reaction)
        .build()
        .await?;
    drasi.start().await?;
    let outcome: Result<()> = async {
        ready(&drasi).await?;
        println!("{}", json!({"ready": true}));
        let mut input = BufReader::new(tokio::io::stdin()).lines();
        input.next_line().await?;
        Ok(())
    }
    .await;
    drasi.stop().await?;
    outcome
}
