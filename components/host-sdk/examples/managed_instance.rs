// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Open/restore managed definitions using a trusted, explicitly supplied native
//! plugin. Production hosts should verify artifacts with the Host SDK lifecycle
//! APIs before loading them. This example never downloads code.

use std::{path::Path, sync::Arc};

use anyhow::{Context, Result};
use drasi_host_sdk::{computation, management::HostManagementResources, PluginRegistry};
use drasi_lib::{management::DesiredInstance, DrasiLib};
use drasi_state_store_redb::RedbConfigurationStore;

#[tokio::main(flavor = "current_thread")]
#[allow(clippy::print_stdout)]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(args.len() == 5 || args.len() == 7,
        "usage: managed_instance <trusted-native-library> <database> <32-byte-key-file> <instance-id> [desired.json request-id]");
    let key = std::fs::read(&args[3]).context("read externally provisioned key")?;
    let key: [u8; 32] = key
        .try_into()
        .map_err(|_| anyhow::anyhow!("key file must contain exactly 32 bytes"))?;
    let store = Arc::new(RedbConfigurationStore::new(&args[2], key)?);
    let mut plugins = PluginRegistry::new();
    plugins.register_computation_plugin(computation::load(Path::new(&args[1]))?)?;
    let resources = Arc::new(HostManagementResources::new(
        &plugins,
        Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
        None,
    )?);
    let instance = DrasiLib::builder()
        .with_id(&args[4])
        .with_component_factories(plugins.computation_factory_registry()?)
        .with_management_resources(resources)
        .with_configuration_store(store)
        .build()
        .await?;
    let operation: Result<()> = async {
        if args.len() == 7 {
            let desired: DesiredInstance = serde_json::from_slice(&std::fs::read(&args[5])?)?;
            let receipt = instance
                .apply_desired_state(
                    instance.desired_configuration()?.revision,
                    &args[6],
                    desired,
                )
                .await?;
            println!("{}", serde_json::to_string(&receipt)?);
        }
        let status = instance.reconcile_desired_state().await?;
        // Do not print configuration or arbitrary constructor errors: both can
        // contain credentials. Use privileged inspection APIs when needed.
        println!(
            "revision={} persistent={} converged={}",
            status.revision,
            status.persistent,
            status.converged()
        );
        Ok(())
    }
    .await;
    let shutdown = instance.shutdown().await.map_err(anyhow::Error::from);
    match (operation, shutdown) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("shutdown also failed: {cleanup}")))
        }
    }
}
