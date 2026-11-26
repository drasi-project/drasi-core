// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Seed a DrasiLib instance from a YAML graph definition and run it.
//!
//! The file is a `DesiredInstance`: DrasiLib's complete stored definition of an
//! instance's components and connections. It is committed to an empty
//! configuration store with `RedbConfigurationStore::initialize_if_absent`, and
//! the builder constructs the graph from that store, as a host does when it
//! starts from a seed. The store is temporary. This host only loads the native
//! plugin libraries named on the command line; it never downloads code.
//!
//! ```sh
//! cargo run --offline -p drasi-host-sdk --example yaml_pipeline -- \
//!   components/host-sdk/examples/source-query-reaction.yaml 8 \
//!   target/debug/libdrasi_computation_source_counter.dylib \
//!   target/debug/libdrasi_computation_transformer_query.dylib \
//!   target/debug/libdrasi_computation_sink_log.dylib
//! ```

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use drasi_host_sdk::{computation, PluginRegistry};
use drasi_lib::{management::DesiredInstance, DrasiLib};
use drasi_state_store_redb::RedbConfigurationStore;

const INSTANCE: &str = "yaml-pipeline";

#[tokio::main]
#[allow(clippy::print_stdout)]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        args.len() >= 4,
        "usage: yaml_pipeline <seed.yaml> <run-seconds> <native-plugin-library>..."
    );
    let text = std::fs::read_to_string(&args[1]).with_context(|| format!("read {}", args[1]))?;
    let seed: DesiredInstance =
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", args[1]))?;
    let run_for = Duration::from_secs(args[2].parse().context("run-seconds")?);

    let mut plugins = PluginRegistry::new();
    for library in &args[3..] {
        plugins.register_computation_plugin(
            computation::load(Path::new(library)).with_context(|| format!("load {library}"))?,
        )?;
    }

    // The random key only protects this throwaway database.
    let directory = tempfile::tempdir()?;
    let mut key = [0; 32];
    key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let store = RedbConfigurationStore::new(directory.path().join("configuration.redb"), key)?;
    anyhow::ensure!(
        store.initialize_if_absent(INSTANCE, "seed", &seed).await?,
        "the configuration store already holds a definition"
    );

    let mut builder = DrasiLib::builder()
        .with_id(INSTANCE)
        .with_component_factories(plugins.computation_factory_registry()?)
        .with_configuration_store(Arc::new(store));
    if let Some(queries) = plugins.query_factory()? {
        builder = builder.with_query_factory(queries);
    }
    let instance = builder.build().await?;

    let run: Result<()> = async {
        instance.start().await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let status = instance.management_status().await?;
            if status.converged() {
                break;
            }
            if let Some(error) = status.error {
                anyhow::bail!("the pipeline did not start: {error}");
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "the pipeline did not become ready within 30s"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        println!("pipeline running for {}s", run_for.as_secs());
        tokio::time::sleep(run_for).await;
        Ok(())
    }
    .await;
    let shutdown = instance.shutdown().await.map_err(anyhow::Error::from);
    match (run, shutdown) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("shutdown also failed: {cleanup}")))
        }
    }
}
