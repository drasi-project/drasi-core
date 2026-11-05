// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use async_trait::async_trait;
use drasi_computation_contracts::{
    workers::{
        join_owned_worker, spawn_owned_component_worker, spawn_owned_scoped_worker,
        WorkerCompletion,
    },
    ComponentDescriptor, ComponentRole, ImplementationIdentity,
};
use drasi_computation_plugin_sdk::{self as sdk, abi, telemetry::*, transport, wire};
use metrics::{Counter, Gauge, Histogram};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{sync::RwLock, task::JoinHandle};
use tracing::Instrument;

type Key = (String, String, u64);
fn handles() -> &'static Mutex<BTreeMap<Key, Counter>> {
    static HANDLES: OnceLock<Mutex<BTreeMap<Key, Counter>>> = OnceLock::new();
    HANDLES.get_or_init(Default::default)
}

#[derive(Debug, Serialize, Deserialize)]
struct Observation {
    instance: String,
    component: String,
    generation: u64,
    context: Option<TraceContextV1>,
}
fn observations() -> &'static Mutex<Vec<Observation>> {
    static OBSERVATIONS: OnceLock<Mutex<Vec<Observation>>> = OnceLock::new();
    OBSERVATIONS.get_or_init(Default::default)
}

struct Factory;
impl sdk::Factory for Factory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("test/telemetry", "1")
                .expect("literal identity"),
            role: ComponentRole::Service,
            configuration_version: 1,
            configuration: sdk::ConfigSchema {
                fields: Default::default(),
                allow_additional: true,
            },
            ports: vec![],
            completion: None,
            capabilities: Default::default(),
        }
    }
    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        let emitter = ScopedMetricEmitter::current()?;
        let counter = emitter.counter(
            "frames_received",
            MetricUnit::One,
            "Frames observed in this binding",
        )?;
        let repeated = emitter.counter(
            "frames_received",
            MetricUnit::One,
            "Frames observed in this binding",
        )?;
        let gauge = emitter.gauge("queued_frames", MetricUnit::One, "Queued frames")?;
        let histogram = emitter.histogram(
            "decode.duration",
            MetricUnit::Seconds,
            "Frame decode duration",
        )?;
        if let Some(binding) = ExecutionScope::current_plugin_telemetry() {
            let config = binding.config();
            handles()
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture handles poisoned"))?
                .insert(
                    (
                        config.scope.instance_id().into(),
                        request.id.to_string(),
                        config.generation,
                    ),
                    counter.clone(),
                );
        }
        log::info!(target: "telemetry_fixture", "constructed");
        anyhow::ensure!(
            request
                .configuration
                .get("fail")
                .and_then(serde_json::Value::as_bool)
                != Some(true),
            "requested constructor failure"
        );
        let absolute = request
            .configuration
            .get("absolute")
            .map(|value| {
                value
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("absolute must be unsigned"))
            })
            .transpose()?
            .unwrap_or(0);
        Ok(sdk::CreatedComponent {
            component: sdk::Component::Service(Box::new(Probe {
                descriptor: self.metadata().descriptor(request.id.clone())?,
                configuration: request.configuration.clone(),
                counter,
                repeated,
                gauge,
                histogram,
                absolute,
                worker: RwLock::new(None),
            })),
            control_handler: None,
        })
    }
}

struct Probe {
    descriptor: ComponentDescriptor,
    configuration: serde_json::Value,
    counter: Counter,
    repeated: Counter,
    gauge: Gauge,
    histogram: Histogram,
    absolute: u64,
    worker: RwLock<Option<JoinHandle<anyhow::Result<()>>>>,
}

async fn join(slot: &RwLock<Option<JoinHandle<anyhow::Result<()>>>>) -> anyhow::Result<()> {
    match join_owned_worker(&mut *slot.write().await, Duration::from_secs(2)).await? {
        WorkerCompletion::Completed(result) => result,
        WorkerCompletion::Absent => Ok(()),
        WorkerCompletion::Cancelled => anyhow::bail!("fixture worker cancelled"),
    }
}

#[async_trait]
impl sdk::NativeComponent for Probe {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        spawn_owned_component_worker(&self.worker, async {
            anyhow::ensure!(
                ExecutionContext::current().is_empty(),
                "component worker inherited startup trace"
            );
            log::info!(target: "telemetry_fixture", "worker.started");
            Ok(())
        })
        .await?;
        join(&self.worker).await
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        join(&self.worker).await?;
        log::info!(target: "telemetry_fixture", "stopped");
        Ok(())
    }
}

#[async_trait]
impl sdk::NativeService for Probe {
    async fn run(&mut self) -> anyhow::Result<()> {
        let (counter, repeated, gauge, histogram, absolute) = (
            self.counter.clone(),
            self.repeated.clone(),
            self.gauge.clone(),
            self.histogram.clone(),
            self.absolute,
        );
        spawn_owned_scoped_worker(&self.worker, async move {
            let span = tracing::info_span!(target: "telemetry_fixture", "plugin.decode", frames = absolute);
            async move {
                tokio::task::yield_now().await;
                counter.absolute(absolute);
                repeated.absolute(absolute);
                gauge.set(7.0);
                histogram.record(0.25);
                tracing::info!(target: "telemetry_fixture", decoded = true, "decoded");
                if let Some(scope) = ExecutionScope::current_component() {
                    let mut values = observations().lock().map_err(|_| anyhow::anyhow!("fixture observations poisoned"))?;
                    anyhow::ensure!(values.len() < 1024, "fixture observation capacity");
                    values.push(Observation {
                        instance: scope.identity.instance_id().into(),
                        component: scope.identity.component_id().expect("component scope").to_string(),
                        generation: scope.generation, context: ExecutionContext::current().trace().cloned(),
                    });
                }
                Ok(())
            }.instrument(span).await
        }).await?;
        join(&self.worker).await
    }
}

fn definition() -> anyhow::Result<sdk::PluginDefinition> {
    sdk::PluginDefinition::new("test/telemetry", "1", vec![Arc::new(Factory)], vec![])
}
sdk::export_computation_plugin!(definition());

#[no_mangle]
/// # Safety
/// `input` is a valid borrowed MessagePack tuple for this synchronous call.
pub unsafe extern "C" fn drasi_telemetry_fixture_update(input: abi::BorrowedBytes) -> abi::Status {
    transport::status_boundary(|| {
        let (instance, component, generation, absolute, value): (String, String, u64, bool, u64) =
            wire::decode(unsafe { transport::borrowed_bytes(input, 4096)? })
                .map_err(transport::Failure::from)?;
        let handles = handles()
            .lock()
            .map_err(|_| transport::Failure::failed("fixture handles poisoned"))?;
        let counter = handles
            .get(&(instance, component, generation))
            .ok_or_else(|| transport::Failure::failed("unknown retained fixture handle"))?;
        if absolute {
            counter.absolute(value);
        } else {
            counter.increment(value);
        }
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn drasi_telemetry_fixture_observations() -> abi::Reply {
    transport::reply_boundary(|| {
        let values = observations()
            .lock()
            .map_err(|_| transport::Failure::failed("fixture observations poisoned"))?;
        wire::encode(&*values).map_err(transport::Failure::from)
    })
}

#[no_mangle]
pub extern "C" fn drasi_telemetry_fixture_clear() -> abi::Status {
    transport::status_boundary(|| {
        handles()
            .lock()
            .map_err(|_| transport::Failure::failed("fixture handles poisoned"))?
            .clear();
        observations()
            .lock()
            .map_err(|_| transport::Failure::failed("fixture observations poisoned"))?
            .clear();
        Ok(())
    })
}
