// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Test-only sink whose owned worker panics on request (RCA-8).
//!
//! `handle` records every input in the configured file before it returns, so
//! the sink's `Handled` completion is honest. For the input whose sequence is
//! `panic_at`, `handle` also tells its owned worker to panic and returns
//! without waiting: the panic happens while no operation of the component is
//! in flight, exactly like a failing listener or replication task of an idle
//! production sink. After recording an input the sink notifies its upstream
//! peer (`fixture.handled`), so a test observes completion without polling.

use std::{io::Write, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context;
use async_trait::async_trait;
use drasi_computation_contracts::{
    workers::{join_owned_worker, spawn_owned_worker, WorkerCleanupError},
    ComponentDescriptor, ComponentRole, GraphChangeCodec, ImplementationIdentity, InputEnvelope,
    PipeRequirements, PortDescriptor, PortDirection, PortId, SinkCompletion,
};
use drasi_computation_plugin_sdk::{self as sdk, lifecycle};
use tokio::{
    sync::{mpsc, RwLock},
    task::JoinHandle,
};

pub const IMPLEMENTATION: &str = "fixture/worker-failure-sink";

pub fn plugin() -> anyhow::Result<sdk::PluginDefinition> {
    sdk::PluginDefinition::new(
        "fixture/native-worker-failure",
        "1",
        vec![Arc::new(Factory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(not(test))]
sdk::export_computation_plugin!(plugin());

pub struct Factory;

impl sdk::Factory for Factory {
    /// Every field described (fixture).
    fn configuration_details(&self) -> sdk::ConfigDetails {
        sdk::ConfigDetails {
            fields: sdk::Factory::metadata(self)
                .configuration
                .fields
                .into_keys()
                .map(|name| {
                    let details = sdk::configuration::ConfigFieldDetails {
                        description: format!("Fixture field {name}."),
                        ..Default::default()
                    };
                    (name, details)
                })
                .collect(),
        }
    }
    fn describe(
        &self,
        _request: &sdk::wire::DescribeRequest,
    ) -> anyhow::Result<sdk::ComponentDescription> {
        Ok(sdk::ComponentDescription::undeclared())
    }
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new(IMPLEMENTATION, "1")
                .expect("fixture identity"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: sdk::ConfigSchema::from_fields([
                ("path", sdk::ConfigType::String, true),
                ("panic_at", sdk::ConfigType::Integer, true),
            ]),
            ports: vec![PortDescriptor::new(
                PortId::try_new("in").expect("fixture port"),
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: Some(SinkCompletion::Handled),
            capabilities: sdk::Capabilities {
                control: true,
                ..Default::default()
            },
        }
    }
    fn create(
        &self,
        request: &sdk::CreateRequest,
        control: sdk::ControlSender,
        _services: sdk::ServiceBindings,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        let configuration = request.configuration.clone();
        let path = PathBuf::from(
            configuration["path"]
                .as_str()
                .context("worker fixture requires a path")?,
        );
        let panic_at = configuration["panic_at"]
            .as_u64()
            .context("worker fixture requires panic_at")?;
        Ok(sdk::CreatedComponent {
            component: sdk::Component::Sink(Box::new(WorkerSink {
                descriptor: sdk::Factory::metadata(self).descriptor(request.id.clone())?,
                configuration,
                path,
                panic_at,
                control,
                commands: None,
                worker: RwLock::new(None),
            })),
            control_handler: Some(Arc::new(IgnorePeers)),
        })
    }
}

/// Peer readiness messages need no reaction from this sink.
struct IgnorePeers;
#[async_trait]
impl sdk::NativeControlHandler for IgnorePeers {
    async fn on_message(
        &self,
        _: sdk::ControlMessage,
        _: sdk::ControlSender,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

struct WorkerSink {
    descriptor: ComponentDescriptor,
    configuration: serde_json::Value,
    path: PathBuf,
    panic_at: u64,
    control: sdk::ControlSender,
    commands: Option<mpsc::UnboundedSender<u64>>,
    worker: RwLock<Option<JoinHandle<()>>>,
}

#[async_trait]
impl sdk::NativeComponent for WorkerSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }
    async fn initialize(&mut self, _: &lifecycle::InitializeContext) -> lifecycle::LifecycleResult {
        Ok(())
    }
    async fn start(&mut self, _: &mut lifecycle::StartContext) -> lifecycle::LifecycleResult {
        let (sender, mut commands) = mpsc::unbounded_channel::<u64>();
        let panic_at = self.panic_at;
        spawn_owned_worker(&self.worker, async move {
            while let Some(sequence) = commands.recv().await {
                if sequence == panic_at {
                    panic!("injected worker panic at sequence {sequence}");
                }
            }
        })
        .await
        .map_err(anyhow::Error::from)?;
        self.commands = Some(sender);
        Ok(())
    }
    async fn pause(&mut self, _: &lifecycle::PauseContext) -> lifecycle::LifecycleResult {
        Ok(())
    }
    async fn resume(&mut self, _: &lifecycle::ResumeContext) -> lifecycle::LifecycleResult {
        Ok(())
    }
    async fn stop(&mut self, _: &lifecycle::StopContext) -> lifecycle::LifecycleResult {
        self.commands = None;
        match join_owned_worker(self.worker.get_mut(), Duration::from_secs(5)).await {
            // The panic was reported to the graph when it happened.
            Ok(_) | Err(WorkerCleanupError::Join(_)) => Ok(()),
            Err(error) => Err(lifecycle::LifecycleError::retryable(error)),
        }
    }
    async fn dispose(&mut self, _: &lifecycle::DisposeContext) -> lifecycle::LifecycleResult {
        Ok(())
    }
}

#[async_trait]
impl sdk::NativeSink for WorkerSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        let sequence = input.envelope.system().sequence();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{sequence}")?;
        file.sync_all()?;
        self.commands
            .as_ref()
            .context("worker fixture is not running")?
            .send(sequence)
            .map_err(|_| anyhow::anyhow!("worker fixture worker has exited"))?;
        self.control.send(
            sdk::ControlTarget::Upstream,
            sdk::ControlNotification::Custom {
                kind: "fixture.handled".into(),
                payload: serde_json::json!(sequence),
            },
        )?;
        Ok(())
    }
}
