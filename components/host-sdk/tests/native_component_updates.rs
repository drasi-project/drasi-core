// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Run `make build-test-plugins` before this target.
//! The test loads real libraries; it does not link their implementations.

use anyhow::{Context, Result};
use drasi_host_sdk::{computation, PluginRegistry};
use drasi_lib::{computation::v1::*, DrasiLib};
use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::Duration};

fn library(package: &str) -> Result<PathBuf> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("components directory")?
        .parent()
        .context("workspace directory")?
        .to_owned();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    let target = if target.is_absolute() {
        target
    } else {
        workspace.join(target)
    };
    let directory = std::env::var_os("DRASI_NATIVE_PLUGIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| target.join("debug/plugins"));
    let path = directory.join(format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        package.replace('-', "_"),
        std::env::consts::DLL_SUFFIX,
    ));
    anyhow::ensure!(
        path.is_file(),
        "first run make build-test-plugins; missing {package} library {}",
        path.display()
    );
    Ok(path)
}

async fn converge(core: &DrasiLib, phase: &str) -> Result<()> {
    let mut last_status = None;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            for (id, component) in &core.computation_control()?.observed().components {
                if let Some(failure) = &component.failure {
                    anyhow::bail!("{phase}: component {id} failed: {failure:?}");
                }
            }
            let status = core.reconcile_desired_state().await?;
            let converged = status.converged();
            last_status = Some(status);
            if converged && core.desired_configuration()?.pending_updates.is_empty() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .with_context(|| {
        format!(
            "{phase}: native graph did not converge; {last_status:?}; observed={:?}",
            core.computation_control().map(|control| control.observed())
        )
    })?
}

async fn managed_update_case() -> Result<()> {
    let plugin = computation::load(library("drasi-computation-source-counter")?)?;
    assert_eq!(
        plugin.metadata().abi_version,
        drasi_computation_plugin_abi::ABI_VERSION
    );
    let counter = plugin
        .factories()
        .first()
        .context("counter factory missing")?
        .clone();
    let capture_plugin = computation::load(library("drasi-computation-sink-capture")?)?;
    let capture = capture_plugin
        .factories()
        .first()
        .context("capture factory missing")?
        .clone();
    let directory = tempfile::tempdir()?;
    let destination = directory.path().join("captured.jsonl");
    let mut registry = PluginRegistry::new();
    registry.register_computation_plugin(plugin)?;
    registry.register_computation_plugin(capture_plugin)?;
    let core = DrasiLib::builder()
        .with_id("native-component-update")
        .with_component_factories(registry.computation_factory_registry()?)
        .build()
        .await?;
    let result = async {
        let graph_id = core.desired_configuration()?.desired.topology.graph_id;
        let created = core
            .create_managed_components(0, "create-pipeline", move |ids| {
                (|| -> Result<_> {
                    let source = ids.allocate()?;
                    let sink = ids.allocate()?;
                    let output = Endpoint::new(source.clone(), PortId::try_new("out")?);
                    let input = Endpoint::new(sink.clone(), PortId::try_new("in")?);
                    let source_specification = counter.specification(
                        source,
                        json!({"stream": "counter/out", "count": 10, "paused": true}),
                    )?;
                    let sink_specification =
                        capture.specification(sink, json!({"path": destination}))?;
                    let graph = ComputationGraph::builder(graph_id)
                        .component(
                            ComponentName::try_new("counter")?,
                            source_specification,
                            counter,
                        )
                        .component(
                            ComponentName::try_new("capture")?,
                            sink_specification,
                            capture,
                        )
                        .bind_stream(output.clone(), StreamId::try_new("counter/out")?)
                        .connect(
                            EdgeDefinition::new(output, input),
                            Box::new(BoundedPipeConfig { capacity: 1 }),
                        )
                        .build()?;
                    Ok(graph.snapshot().select(GraphSelection::All)?)
                })()
                .map_err(|error| GraphError::Topology {
                    reason: error.to_string(),
                })
            })
            .await?;
        let id = created
            .creation
            .as_ref()
            .context("creation receipt missing")?
            .components
            .iter()
            .find(|identity| identity.name.as_str() == "counter")
            .context("assigned counter identity missing")?
            .id
            .clone();
        core.start().await?;
        converge(&core, "initial activation").await?;
        let control = core.computation_control()?;
        let generation = control
            .observed()
            .components
            .get(&id)
            .context("counter not realized")?
            .generation;
        let current = core.desired_configuration()?;
        let mut desired = current.desired.clone();
        let component = desired
            .topology
            .components
            .iter_mut()
            .find(|component| component.descriptor.id() == &id)
            .context("counter recipe missing")?;
        let ComponentConstruction::Factory(specification) = &mut component.construction else {
            anyhow::bail!("counter was not recorded as a factory recipe")
        };
        specification.configuration.insert(
            Arc::from("interval_ms"),
            ConfigurationValue::Literal(json!(20)),
        );
        let accepted = core
            .apply_desired_state(current.revision, "update-counter", desired.clone())
            .await?;
        assert_eq!(accepted.updates.len(), 1);
        assert_eq!(accepted.updates[0].component_id, id);
        assert_eq!(
            accepted.updates[0].plan,
            ComponentUpdatePlan {
                mode: ComponentUpdateMode::InPlace,
                data: ComponentUpdateData::Preserve,
            }
        );
        converge(&core, "interval update").await?;
        assert_eq!(control.observed().components[&id].generation, generation);
        let snapshot = control.configuration_snapshot()?;
        let Some(CapturedComponentConfiguration::Available { values }) =
            snapshot.configurations.get(&id)
        else {
            anyhow::bail!("live counter configuration is unavailable")
        };
        assert_eq!(values["interval_ms"], json!(20));
        assert_eq!(values["paused"], json!(true));
        assert_eq!(
            core.apply_desired_state(current.revision, "update-counter", desired)
                .await?,
            accepted,
        );
        assert!(core.desired_configuration()?.pending_updates.is_empty());
        let mut unsupported = core.desired_configuration()?.desired;
        let component = unsupported
            .topology
            .components
            .iter_mut()
            .find(|component| component.descriptor.id() == &id)
            .context("counter recipe missing")?;
        let ComponentConstruction::Factory(specification) = &mut component.construction else {
            anyhow::bail!("counter lost its factory recipe")
        };
        specification
            .configuration
            .insert(Arc::from("count"), ConfigurationValue::Literal(json!(30)));
        assert!(core
            .apply_desired_state(accepted.revision, "unsupported-update", unsupported)
            .await
            .is_err());
        assert_eq!(core.desired_configuration()?.revision, accepted.revision);
        assert_eq!(control.observed().components[&id].generation, generation);
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let shutdown = core.shutdown().await;
    match (result, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("shutdown also failed: {cleanup}")))
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn real_library_managed_update_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), managed_update_case()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_library_managed_update_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), managed_update_case()).await?
}
