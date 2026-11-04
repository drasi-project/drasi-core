// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use async_trait::async_trait;
use drasi_computation_contracts::{
    emission_id,
    workers::{
        join_owned_worker, spawn_owned_component_worker, spawn_owned_scoped_worker,
        WorkerCompletion,
    },
    ComponentDescriptor, ComponentRole, GraphChangeCodec, ImplementationIdentity, InputEnvelope,
    OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId, SinkBatching,
    SinkCompletion, StreamId, SystemMetadata,
};
use drasi_computation_plugin_sdk::{
    self as sdk,
    telemetry::{
        ExecutionContext, ExecutionScope, OpenTelemetryContextAdapter, TraceContextAdapter,
        TraceContextV1,
    },
    NativeComponent, NativeSink, NativeSource, NativeTransformer,
};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SdkTracerProvider};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{sync::RwLock, task::JoinHandle};
use tracing::Instrument;
use tracing_subscriber::prelude::*;

struct Telemetry {
    provider: SdkTracerProvider,
    exporter: InMemorySpanExporter,
    calls: Mutex<Vec<Value>>,
}

fn telemetry() -> &'static Telemetry {
    static TELEMETRY: OnceLock<Telemetry> = OnceLock::new();
    TELEMETRY.get_or_init(|| {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .with_sampler(Sampler::ParentBased(Box::new(Sampler::AlwaysOn)))
            .build();
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(provider.tracer("native-context-fixture")),
        ))
        .expect("fixture-local subscriber");
        Telemetry {
            provider,
            exporter,
            calls: Mutex::new(Vec::new()),
        }
    })
}

fn trace(context: &ExecutionContext) -> Value {
    context.trace().map_or(Value::Null, |trace| {
        json!({
            "trace": trace.trace_id(), "span": trace.span_id(), "sampled": trace.sampled(),
        })
    })
}

fn record(phase: &str, operation: &ExecutionContext, inputs: &[ExecutionContext]) {
    let component = ExecutionScope::current_component();
    let value = json!({
        "phase": phase,
        "instance": component.as_ref().map(|value| value.identity.instance_id()),
        "graph": component.as_ref().map(|value| value.identity.graph_id()),
        "component": component.as_ref().and_then(|value| value.identity.component_id()).map(ToString::to_string),
        "generation": component.as_ref().map(|value| value.generation),
        "operation": trace(operation),
        "current": trace(&ExecutionContext::current()),
        "inputs": inputs.iter().map(trace).collect::<Vec<_>>(),
    });
    let mut calls = telemetry().calls.lock().expect("fixture observations");
    assert!(calls.len() < 1024, "fixture observation bound");
    calls.push(value);
}

struct OnDrop(ExecutionContext);
impl Drop for OnDrop {
    fn drop(&mut self) {
        record("drop", &self.0, &[]);
    }
}

pub fn definition() -> anyhow::Result<sdk::PluginDefinition> {
    telemetry();
    sdk::PluginDefinition::new(
        "test/context-fixture",
        "1",
        vec![
            Arc::new(Factory(ComponentRole::Source)),
            Arc::new(Factory(ComponentRole::Transformer)),
            Arc::new(Factory(ComponentRole::Sink)),
        ],
        vec![GraphChangeCodec::schema()],
    )
}

struct Factory(ComponentRole);
impl sdk::Factory for Factory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        let name = match self.0 {
            ComponentRole::Source => "test/context-source",
            ComponentRole::Transformer => "test/context-transformer",
            _ => "test/context-sink",
        };
        let directions = match self.0 {
            ComponentRole::Source => vec![PortDirection::Output],
            ComponentRole::Transformer => vec![PortDirection::Input, PortDirection::Output],
            _ => vec![PortDirection::Input],
        };
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new(name, "1").expect("fixture identity"),
            role: self.0,
            configuration_version: 1,
            configuration: sdk::ConfigSchema {
                fields: Default::default(),
                allow_additional: true,
            },
            ports: directions
                .into_iter()
                .map(|direction| {
                    PortDescriptor::new(
                        PortId::try_new(if direction == PortDirection::Input {
                            "in"
                        } else {
                            "out"
                        })
                        .expect("fixture port"),
                        direction,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )
                })
                .collect(),
            completion: (self.0 == ComponentRole::Sink).then_some(SinkCompletion::Handled),
            capabilities: sdk::Capabilities::default(),
        }
    }

    fn supports_sink_batching(&self) -> bool {
        self.0 == ComponentRole::Sink
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        for field in ["trace", "sampled", "block"] {
            anyhow::ensure!(
                request
                    .configuration
                    .get(field)
                    .is_none_or(Value::is_boolean),
                "fixture {field} must be a boolean"
            );
        }
        anyhow::ensure!(
            request.configuration.get("seed").is_none_or(|value| {
                value.as_u64().is_some_and(|seed| (1..=255).contains(&seed))
            }),
            "fixture seed must be in 1..=255"
        );
        record("create", &ExecutionContext::current(), &[]);
        let value = Box::new(Probe {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            configuration: request.configuration.clone(),
            running: false,
            emitted: 0,
            worker: RwLock::new(None),
        });
        Ok(sdk::CreatedComponent {
            component: match self.0 {
                ComponentRole::Source => sdk::Component::Source(value),
                ComponentRole::Transformer => sdk::Component::Transformer(value),
                _ => sdk::Component::Sink(value),
            },
            control_handler: None,
        })
    }
}

struct Probe {
    descriptor: ComponentDescriptor,
    configuration: Value,
    running: bool,
    emitted: u64,
    worker: RwLock<Option<JoinHandle<ExecutionContext>>>,
}

impl Probe {
    fn tracing(&self) -> bool {
        self.configuration["trace"].as_bool().unwrap_or(false)
    }

    async fn worker(&self) -> anyhow::Result<()> {
        let expected = ExecutionContext::current();
        spawn_owned_scoped_worker(&self.worker, async {
            tokio::task::yield_now().await;
            ExecutionContext::current()
        })
        .await?;
        let mut worker = self.worker.write().await;
        match join_owned_worker(&mut worker, Duration::from_secs(2)).await? {
            WorkerCompletion::Completed(actual) => {
                anyhow::ensure!(actual == expected, "native worker lost context");
            }
            _ => anyhow::bail!("native worker did not complete"),
        }
        drop(worker);
        let expected_component = ExecutionScope::current_component();
        spawn_owned_component_worker(&self.worker, async move {
            tokio::task::yield_now().await;
            let actual = ExecutionScope::current_component();
            assert_eq!(
                actual.as_ref().map(|scope| scope.generation),
                expected_component.as_ref().map(|scope| scope.generation)
            );
            assert_eq!(
                actual
                    .as_ref()
                    .and_then(|scope| scope.identity.component_id()),
                expected_component
                    .as_ref()
                    .and_then(|scope| scope.identity.component_id()),
            );
            ExecutionContext::current()
        })
        .await?;
        let mut worker = self.worker.write().await;
        match join_owned_worker(&mut worker, Duration::from_secs(2)).await? {
            WorkerCompletion::Completed(actual) => {
                anyhow::ensure!(
                    actual.is_empty(),
                    "native component worker inherited a lifecycle parent"
                );
            }
            _ => anyhow::bail!("native component worker did not complete"),
        }
        Ok(())
    }
}

#[async_trait]
impl NativeComponent for Probe {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<Value> {
        Ok(self.configuration.clone())
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.running = true;
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        let mut worker = self.worker.write().await;
        join_owned_worker(&mut worker, Duration::from_secs(2)).await?;
        self.running = false;
        Ok(())
    }
}

#[async_trait]
impl NativeSource for Probe {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        anyhow::ensure!(self.running, "source stopped");
        if self.emitted == 1 {
            return Ok(None);
        }
        self.emitted += 1;
        let span = if self.tracing() {
            let span = tracing::info_span!("fixture.source");
            let seed = u8::try_from(self.configuration["seed"].as_u64().unwrap_or(1))?;
            let parent = ExecutionContext::new(TraceContextV1::new(
                [seed; 16],
                [seed; 8],
                u8::from(self.configuration["sampled"].as_bool().unwrap_or(true)),
                None,
            )?);
            OpenTelemetryContextAdapter.set_parent(&span, &parent)?;
            span
        } else {
            tracing::Span::none()
        };
        async {
            self.worker().await?;
            let envelope = GraphChangeCodec::encode_change(
                SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("fixture", "1"),
                            labels: Arc::from([Arc::from("Item")]),
                            effective_from: 1,
                        },
                        properties: ElementPropertyMap::default(),
                    },
                },
                StreamId::try_new("source/out")?,
                1,
                None,
            )?;
            record("source", &ExecutionContext::current(), &[]);
            let output = OutputEnvelope::new(PortId::try_new("out")?, envelope);
            Ok(Some(if self.tracing() {
                output.with_current_context()
            } else {
                output
            }))
        }
        .instrument(span)
        .await
    }
}

#[async_trait]
impl NativeTransformer for Probe {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(self.running, "transformer stopped");
        let operation = ExecutionContext::current();
        let span = if self.tracing() {
            tracing::info_span!("fixture.transform")
        } else {
            tracing::Span::none()
        };
        async {
            let _drop = OnDrop(operation.clone());
            record(
                "transform",
                &operation,
                std::slice::from_ref(&input.execution_context),
            );
            self.worker().await?;
            if self.configuration["block"].as_bool().unwrap_or(false) {
                std::future::pending::<()>().await;
            }
            self.emitted += 1;
            let stream = StreamId::try_new("middle/out")?;
            let envelope = input.envelope.derive(
                emission_id(&stream, self.emitted)?,
                input.envelope.changes().clone(),
                SystemMetadata::new(stream, self.emitted),
            );
            let output = OutputEnvelope::new(PortId::try_new("out")?, envelope);
            Ok(vec![if self.tracing() {
                output.with_current_context()
            } else {
                output.with_execution_context(input.execution_context)
            }])
        }
        .instrument(span)
        .await
    }
}

#[async_trait]
impl NativeSink for Probe {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn batching(&self) -> anyhow::Result<Option<SinkBatching>> {
        Ok(Some(SinkBatching {
            max_inputs: 8,
            max_operations: 8,
            max_bytes: 64 * 1024,
            max_delay_ms: 10,
        }))
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.handle_batch(vec![input]).await
    }
    async fn handle_batch(&mut self, inputs: Vec<InputEnvelope>) -> anyhow::Result<()> {
        anyhow::ensure!(self.running, "sink stopped");
        let operation = ExecutionContext::current();
        let span = if self.tracing() {
            tracing::info_span!("fixture.sink")
        } else {
            tracing::Span::none()
        };
        async {
            let contexts = inputs
                .iter()
                .map(|input| input.execution_context.clone())
                .collect::<Vec<_>>();
            self.worker().await?;
            record("sink", &operation, &contexts);
            Ok(())
        }
        .instrument(span)
        .await
    }
}

#[no_mangle]
pub extern "C" fn drasi_execution_context_test_observations() -> sdk::abi::Reply {
    sdk::transport::reply_boundary(|| {
        telemetry()
            .provider
            .force_flush()
            .map_err(anyhow::Error::from)?;
        let spans = telemetry()
            .exporter
            .get_finished_spans()
            .map_err(anyhow::Error::from)?;
        let spans = spans.iter().map(|span| json!({
            "name": span.name, "trace": span.span_context.trace_id().to_bytes(),
            "span": span.span_context.span_id().to_bytes(), "parent": span.parent_span_id.to_bytes(),
        })).collect::<Vec<_>>();
        sdk::wire::encode(&json!({
            "calls": *telemetry().calls.lock().expect("fixture observations"),
            "spans": spans,
            "outside_context": trace(&OpenTelemetryContextAdapter.current_context()),
        }))
        .map_err(sdk::transport::Failure::from)
    })
}
