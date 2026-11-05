// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ChangeOperation, ComponentDescriptor, ComponentRole, EnvelopeId, ImplementationIdentity,
    InputEnvelope, OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId,
    QueryChangeCodec, StreamId, SystemMetadata,
};
use drasi_computation_plugin_sdk::{
    self as sdk,
    telemetry::{MetricUnit, ScopedMetricEmitter},
};
use metrics::{Counter, Gauge, Histogram};
use serde_json::Value;
use std::{sync::Arc, time::Instant};

struct Factory;

impl sdk::Factory for Factory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("drasi.demo/inspector", "1")
                .expect("constant implementation identity"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: sdk::ConfigSchema {
                fields: Default::default(),
                allow_additional: false,
            },
            ports: [("in", PortDirection::Input), ("out", PortDirection::Output)]
                .into_iter()
                .map(|(name, direction)| {
                    PortDescriptor::new(
                        PortId::try_new(name).expect("constant port"),
                        direction,
                        QueryChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )
                })
                .collect(),
            completion: None,
            capabilities: Default::default(),
        }
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> Result<sdk::CreatedComponent> {
        let emitter = ScopedMetricEmitter::current()?;
        let inspector = Inspector {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            stream: StreamId::try_new(format!("{}/out", request.id))?,
            bytes: emitter.counter(
                "record_bytes_inspected",
                MetricUnit::One,
                "Serialized record bytes checksummed by the demo inspector",
            )?,
            duration: emitter.histogram(
                "inspection.duration",
                MetricUnit::Seconds,
                "Time spent inspecting serialized record images",
            )?,
            active: emitter.gauge(
                "inspection_active",
                MetricUnit::One,
                "Whether this demo inspector is inspecting records",
            )?,
            sequence: 0,
            running: false,
        };
        tracing::info!(target: "drasi_demo_plugin", "Native inspector constructed with scoped metric handles");
        Ok(sdk::Component::Transformer(Box::new(inspector)).into())
    }
}

struct Inspector {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    bytes: Counter,
    duration: Histogram,
    active: Gauge,
    sequence: u64,
    running: bool,
}

struct Inspection<'a> {
    active: &'a Gauge,
    duration: &'a Histogram,
    started: Instant,
}

impl Drop for Inspection<'_> {
    fn drop(&mut self) {
        self.duration.record(self.started.elapsed().as_secs_f64());
        self.active.set(0.0);
    }
}

#[async_trait]
impl sdk::NativeComponent for Inspector {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> Result<Value> {
        Ok(serde_json::json!({}))
    }

    async fn start(&mut self) -> Result<()> {
        self.running = true;
        tracing::info!(target: "drasi_demo_plugin", "Native inspector started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.running = false;
        self.active.set(0.0);
        tracing::info!(target: "drasi_demo_plugin", "Native inspector stopped");
        Ok(())
    }
}

#[async_trait]
impl sdk::NativeTransformer for Inspector {
    async fn transform(&mut self, input: InputEnvelope) -> Result<Vec<OutputEnvelope>> {
        ensure!(
            self.running && input.port.as_str() == "in",
            "inspector is not running on its input port"
        );
        let sequence = self
            .sequence
            .checked_add(1)
            .context("demo output sequence exhausted")?;
        let output = {
            let span = tracing::info_span!(
                target: "drasi_demo_plugin",
                "plugin.inspect",
                record_bytes = tracing::field::Empty,
                record_images = tracing::field::Empty,
                outcome = "success"
            );
            let _entered = span.enter();
            self.active.set(1.0);
            let _inspection = Inspection {
                active: &self.active,
                duration: &self.duration,
                started: Instant::now(),
            };
            let mut bytes = 0u64;
            let mut images = 0u64;
            let mut checksum = 0u64;
            for operation in input.envelope.changes().operations() {
                let records = match operation {
                    ChangeOperation::Added { after, .. } => [None, Some(after)],
                    ChangeOperation::Updated { before, after, .. } => {
                        [before.as_ref(), Some(after)]
                    }
                    ChangeOperation::Deleted { before, .. } => [before.as_ref(), None],
                };
                for record in records.into_iter().flatten() {
                    bytes = bytes
                        .checked_add(u64::try_from(record.payload().len())?)
                        .context("inspection byte count overflow")?;
                    images += 1;
                    for byte in record.payload() {
                        checksum = checksum.rotate_left(5) ^ u64::from(*byte);
                    }
                }
            }
            std::hint::black_box(checksum);
            self.bytes.increment(bytes);
            span.record("record_bytes", bytes);
            span.record("record_images", images);
            tracing::info!(target: "drasi_demo_plugin", record_bytes = bytes, record_images = images, "Inspected serialized record images");
            let mut metadata = SystemMetadata::new(self.stream.clone(), sequence);
            if let Some(timestamp) = input.envelope.system().timestamp() {
                metadata = metadata.with_timestamp(timestamp);
            }
            let envelope = input.envelope.try_derive(
                EnvelopeId::try_new(self.stream.as_str(), sequence.to_be_bytes().to_vec().into())?,
                input.envelope.changes().clone(),
                metadata,
            )?;
            OutputEnvelope::new(PortId::try_new("out")?, envelope).with_current_context()
        };
        self.sequence = sequence;
        Ok(vec![output])
    }
}

fn plugin() -> Result<sdk::PluginDefinition> {
    sdk::PluginDefinition::new(
        "drasi.demo/observability",
        "1",
        vec![Arc::new(Factory)],
        vec![QueryChangeCodec::schema()],
    )
}

sdk::export_computation_plugin!(plugin());
