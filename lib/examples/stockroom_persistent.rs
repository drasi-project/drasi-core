// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Query-state and source-WAL companion to docs/developer-guide/README.md.
//! Run with `-- <directory> write`, then `-- <directory> read`.
//! Use a new directory for the write step. Read never submits an input event.

#![allow(clippy::print_stdout)]

use anyhow::{ensure, Context};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{
    wal::{CapacityPolicy, DurabilityConfig},
    DrasiLib, Query, RecoveryPolicy, StorageBackendRef,
};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use drasi_wal_redb::RedbWalProvider;
use std::{path::Path, sync::Arc, time::Duration};

async fn stockroom(directory: &Path, write: bool) -> anyhow::Result<()> {
    std::fs::create_dir_all(directory)?;
    let (source, input) = ApplicationSource::new(
        "inventory",
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: Some(DurabilityConfig {
                enabled: true,
                max_events: 10_000,
                capacity_policy: CapacityPolicy::RejectIncoming,
            }),
        },
    )?;
    let (reaction, output) = ApplicationReaction::new("alerts", vec!["low-stock".into()]);
    let mut receiver = output
        .take_receiver()
        .await
        .context("alerts already taken")?;
    let indexes = RocksDbIndexProvider::new(directory.join("indexes"), false, false)
        .with_memory_budget_bytes(128 << 20)?;
    let core = DrasiLib::builder()
        .with_id("stockroom-persistent")
        .with_index_provider("local", Arc::new(indexes))
        .with_wal_provider(Arc::new(RedbWalProvider::new(directory.join("wal"))))
        .with_source(source)
        .with_query(
            Query::cypher("low-stock")
                .query(
                    "MATCH (p:Product) WHERE p.quantity < p.minimum
                     RETURN p.sku AS sku, p.quantity AS quantity",
                )
                .from_source("inventory")
                .enable_bootstrap(false)
                .with_storage_backend(StorageBackendRef::Named("local".into()))
                .with_recovery_policy(RecoveryPolicy::Strict)
                .build(),
        )
        .with_reaction(reaction)
        .build()
        .await?;
    let work = async {
        core.start().await?;
        tokio::time::timeout(
            Duration::from_secs(10),
            core.computation_component("alerts")?.wait_started(),
        )
        .await??;
        if write {
            ensure!(
                core.get_query_results("low-stock").await?.is_empty(),
                "write requires a new stockroom; use read to inspect saved data"
            );
            input
                .send_node_insert(
                    "bolts",
                    vec!["Product"],
                    PropertyMapBuilder::new()
                        .with_string("sku", "bolts")
                        .with_integer("quantity", 3)
                        .with_integer("minimum", 5)
                        .build(),
                )
                .await?;
            tokio::time::timeout(Duration::from_secs(10), receiver.recv())
                .await?
                .context("alerts closed before the write was processed")?;
        }
        let rows = core.get_query_results("low-stock").await?;
        ensure!(
            rows == vec![serde_json::json!({"sku": "bolts", "quantity": 3})],
            "expected saved low-stock product, got {rows:?}"
        );
        println!("{}: {rows:?}", if write { "written" } else { "restored" });
        Ok(())
    }
    .await;
    let cleanup = core.shutdown().await;
    if let Err(error) = &cleanup {
        eprintln!("Stockroom shutdown failed: {error:#}");
    }
    work?;
    cleanup?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let [directory, mode] = args.as_slice() else {
        anyhow::bail!("usage: stockroom_persistent <directory> <write|read>");
    };
    let write = match mode.as_str() {
        "write" => true,
        "read" => false,
        _ => anyhow::bail!("mode must be write or read"),
    };
    stockroom(Path::new(directory), write).await
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn reconstructs_without_resubmitting_inventory() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        super::stockroom(directory.path(), true).await?;
        super::stockroom(directory.path(), false).await
    }
}
