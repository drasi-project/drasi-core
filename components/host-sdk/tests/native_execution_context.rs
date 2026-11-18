// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Build the two SDK context fixture cdylibs before running this target.

#![cfg(test)]
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_plugin_sdk::{abi, transport, wire};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_host_sdk::computation::{self, NativeFactory, NativePlugin};
use drasi_lib::{
    computation::v1::*,
    observability::{
        ExecutionScope, OpenTelemetryContextAdapter, TelemetryOptions, TelemetryProfile,
        TelemetrySettings, TraceContextV1,
    },
};
use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SdkTracerProvider};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::*;

const SOURCE_ID: &str = "e07b5b54-840c-4c48-bac0-0bbdf637d306";
const MIDDLE_ID: &str = "3531a429-8ac1-4c60-985f-82ea080c5281";
const COLLECTED_ID: &str = "f027d30c-f549-4d41-a5cf-bd7f0bc1e606";
const BATCH_SOURCE_ID: &str = "8342ceea-94a0-4511-9ae5-c35d894ebf5f";
const BATCH_SINK_ID: &str = "b9ac4ff2-60b2-4689-ad89-a9e1d37795cf";

fn fixture(base: bool) -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            }
        })
        .unwrap_or_else(|| workspace.join("target"));
    let name = if base {
        "execution_context_base_fixture"
    } else {
        "execution_context_fixture"
    };
    target.join("debug/examples").join(format!(
        "{}{name}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX,
    ))
}

fn load(base: bool) -> Result<Arc<NativePlugin>> {
    let path = fixture(base);
    anyhow::ensure!(path.is_file(),
        "missing fixture {}; run cargo build -p drasi-computation-plugin-sdk --examples --features otel-context --locked",
        path.display());
    computation::load(path)
}

fn factory(plugin: &NativePlugin, kind: &str) -> Arc<NativeFactory> {
    plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == format!("test/context-{kind}")
        })
        .expect("fixture factory")
        .clone()
}

fn context(id: u8, sampled: bool) -> ExecutionContext {
    ExecutionContext::new(TraceContextV1::new([id; 16], [id; 8], u8::from(sampled), None).unwrap())
}

fn input(id: u8) -> InputEnvelope {
    InputEnvelope::new(
        PortId::try_new("in").unwrap(),
        GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", &id.to_string()),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: u64::from(id),
                    },
                    properties: ElementPropertyMap::default(),
                },
            },
            StreamId::try_new("source/out").unwrap(),
            u64::from(id),
            None,
        )
        .unwrap(),
    )
    .with_execution_context(context(id, true))
}

fn observations() -> Result<Value> {
    unsafe {
        let library = libloading::Library::new(fixture(false))?;
        let read = library.get::<unsafe extern "C" fn() -> abi::Reply>(
            b"drasi_execution_context_test_observations\0",
        )?;
        let bytes = transport::take_reply(read())?;
        wire::decode(&bytes)
    }
}

struct Collector {
    descriptor: ComponentDescriptor,
    contexts: Arc<Mutex<Vec<ExecutionContext>>>,
    instance: String,
}

#[async_trait]
impl ComputationComponent for Collector {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for Collector {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        tokio::task::yield_now().await;
        assert_eq!(ExecutionContext::current(), input.execution_context);
        let scope = ExecutionScope::current_component().context("receiver scope")?;
        assert_eq!(scope.identity.instance_id(), self.instance);
        assert_eq!(
            scope.identity.component_id().unwrap().as_str(),
            COLLECTED_ID
        );
        self.contexts.lock().unwrap().push(input.execution_context);
        Ok(())
    }
}

async fn pipeline(plugin: &NativePlugin, instance: &str, seed: u8, sampled: bool) -> Result<()> {
    let source = factory(plugin, "source");
    let middle = factory(plugin, "transformer");
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let endpoint = |component: &str, port: &str| {
        Endpoint::new(
            ComponentId::try_new(component).unwrap(),
            PortId::try_new(port).unwrap(),
        )
    };
    let mut graph = ComputationGraph::builder(format!("graph-{instance}"))
        .with_telemetry(
            TelemetryOptions::new(
                TelemetrySettings::new(TelemetryProfile::Basic)
                    .with_metrics(false)
                    .require_distributed_tracing(),
            )
            .with_instance_id(instance.to_owned())
            .with_context_adapter(Arc::new(OpenTelemetryContextAdapter)),
        )
        .component(
            ComponentName::try_new("source")?,
            source.specification(
                ComponentId::try_new(SOURCE_ID)?,
                json!({
                    "trace": true, "seed": seed, "sampled": sampled,
                }),
            )?,
            source,
        )
        .component(
            ComponentName::try_new("middle")?,
            middle.specification(ComponentId::try_new(MIDDLE_ID)?, json!({"trace": true}))?,
            middle,
        )
        .sink(
            ComponentName::try_new("collected")?,
            Box::new(Collector {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new(COLLECTED_ID)?,
                    vec![PortDescriptor::new(
                        PortId::try_new("in")?,
                        PortDirection::Input,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                contexts: contexts.clone(),
                instance: format!("graph-{instance}"),
            }),
        )
        .bind_stream(endpoint(SOURCE_ID, "out"), StreamId::try_new("source/out")?)
        .bind_stream(endpoint(MIDDLE_ID, "out"), StreamId::try_new("middle/out")?)
        .connect(
            EdgeDefinition::new(endpoint(SOURCE_ID, "out"), endpoint(MIDDLE_ID, "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint(MIDDLE_ID, "out"), endpoint(COLLECTED_ID, "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()?;
    tokio::time::timeout(Duration::from_secs(10), graph.start()?).await??;
    graph.dispose().await?;
    let contexts = contexts.lock().unwrap();
    assert_eq!(contexts.len(), 1);
    assert_eq!(contexts[0].trace().unwrap().trace_id(), [seed; 16]);
    assert_eq!(contexts[0].trace().unwrap().sampled(), sampled);
    Ok(())
}

async fn batch_and_cancellation(plugin: &NativePlugin) -> Result<()> {
    let scope = ExecutionScope::new(context(51, true))
        .with_adapter(Some(Arc::new(OpenTelemetryContextAdapter)));
    let mut sink = scope.in_scope(|| {
        factory(plugin, "sink")
            .create_component(ComponentId::try_new(BATCH_SINK_ID)?, json!({"trace": true}))
    })?;
    sink.initialize().await?;
    sink.start().await?;
    scope
        .scope(sink.handle_batch(vec![input(52), input(53)]))
        .await?;
    sink.stop().await?;
    sink.dispose().await?;

    let mut middle = scope.in_scope(|| {
        factory(plugin, "transformer").create_component(
            ComponentId::try_new("e4cd95ba-c4fd-4a83-8a11-ea66904c2be0")?,
            json!({"trace": true, "block": true}),
        )
    })?;
    middle.initialize().await?;
    middle.start().await?;
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        scope.scope(middle.transform(input(54))),
    )
    .await;
    assert!(
        result.is_err(),
        "pending native callback should be cancelled by its owner"
    );
    middle.stop().await?;
    middle.dispose().await?;
    assert!(ExecutionContext::current().is_empty());
    let observed = observations()?;
    assert!(
        observed["outside_context"].is_null(),
        "native adapter leaked a context after release"
    );
    let calls = observed["calls"].as_array().unwrap();
    let batch = calls
        .iter()
        .find(|call| call["phase"] == "sink" && call["operation"]["trace"] == json!(vec![51u8; 16]))
        .context("native batch observation")?;
    assert_eq!(batch["inputs"][0]["trace"], json!(vec![52u8; 16]));
    assert_eq!(batch["inputs"][1]["trace"], json!(vec![53u8; 16]));
    assert_eq!(batch["current"]["trace"], json!(vec![51u8; 16]));
    assert!(
        calls
            .iter()
            .any(|call| call["phase"] == "drop"
                && call["operation"]["trace"] == json!(vec![51u8; 16]))
    );
    Ok(())
}

async fn graph_created_native_batch(plugin: &NativePlugin, sampled: bool) -> Result<()> {
    struct BatchSource {
        descriptor: ComponentDescriptor,
        outputs: std::collections::VecDeque<OutputEnvelope>,
    }
    #[async_trait]
    impl ComputationComponent for BatchSource {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.descriptor
        }
        async fn start(&mut self) -> Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }
    #[async_trait]
    impl EnvelopeSource for BatchSource {
        async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
            Ok(self.outputs.pop_front())
        }
    }
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .with_sampler(drasi_lib::observability::TelemetrySampler(
            Sampler::AlwaysOn,
        ))
        .build();
    let sink = factory(plugin, "sink");
    let instance = if sampled {
        "batch-sampled"
    } else {
        "batch-unsampled"
    };
    async {
        let outputs = [81u8, 82]
            .into_iter()
            .enumerate()
            .map(|(index, id)| {
                let input = input(id);
                let mut envelope = input.envelope;
                envelope = envelope.derive(
                    emission_id(
                        &StreamId::try_new("batch-source/out").unwrap(),
                        index as u64 + 1,
                    )
                    .unwrap(),
                    envelope.changes().clone(),
                    SystemMetadata::new(
                        StreamId::try_new("batch-source/out").unwrap(),
                        index as u64 + 1,
                    ),
                );
                OutputEnvelope::new(PortId::try_new("out").unwrap(), envelope)
                    .with_execution_context(context(id, index == 0))
            })
            .collect();
        let mut graph = ComputationGraph::builder(instance)
            .with_telemetry(
                TelemetryOptions::new(
                    TelemetrySettings::new(TelemetryProfile::Debug)
                        .with_metrics(false)
                        .with_sampling(if sampled {
                            drasi_lib::observability::RootSampling::Always
                        } else {
                            drasi_lib::observability::RootSampling::Never
                        })
                        .require_distributed_tracing(),
                )
                .with_instance_id(instance)
                .with_context_adapter(Arc::new(OpenTelemetryContextAdapter)),
            )
            .source(
                ComponentName::try_new("batch-source")?,
                Box::new(BatchSource {
                    descriptor: ComponentDescriptor::try_new(
                        ComponentId::try_new(BATCH_SOURCE_ID)?,
                        vec![PortDescriptor::new(
                            PortId::try_new("out")?,
                            PortDirection::Output,
                            GraphChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        )],
                    )?,
                    outputs,
                }),
            )
            .component(
                ComponentName::try_new("batch-sink")?,
                sink.specification(ComponentId::try_new(BATCH_SINK_ID)?, json!({"trace": true}))?,
                sink,
            )
            .bind_stream(
                Endpoint::new(
                    ComponentId::try_new(BATCH_SOURCE_ID)?,
                    PortId::try_new("out")?,
                ),
                StreamId::try_new("batch-source/out")?,
            )
            .connect(
                EdgeDefinition::new(
                    Endpoint::new(
                        ComponentId::try_new(BATCH_SOURCE_ID)?,
                        PortId::try_new("out")?,
                    ),
                    Endpoint::new(ComponentId::try_new(BATCH_SINK_ID)?, PortId::try_new("in")?),
                ),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .build()?;
        let run = graph.run()?;
        let control = run.control();
        let request_scope = ExecutionScope::new(context(93, true))
            .with_adapter(Some(Arc::new(OpenTelemetryContextAdapter)));
        let (driver, applied) =
            tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(run, async {
                    control.deployment_report().await?;
                    request_scope
                        .scope(control.start_components(
                            control.desired_snapshot().revision,
                            GraphSelection::All,
                        ))
                        .await?;
                    let mut observed = control.subscribe_observed();
                    observed
                        .wait_for(|graph| {
                            graph
                                .components
                                .values()
                                .all(|component| component.exhausted)
                        })
                        .await?;
                    control.cancel();
                    Ok::<_, anyhow::Error>(())
                })
            })
            .await?;
        applied?;
        assert!(matches!(driver, Err(GraphError::Cancelled)));
        graph.dispose().await?;
        Ok::<_, anyhow::Error>(())
    }
    .with_subscriber(
        tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("host-operations"))),
    )
    .await?;
    provider.force_flush()?;
    let observations = observations()?;
    let call = observations["calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|call| call["phase"] == "sink" && call["graph"] == instance)
        .context("graph-owned native batch call")?;
    assert_eq!(call["inputs"].as_array().unwrap().len(), 2);
    assert_eq!(call["operation"]["sampled"], sampled);
    assert_ne!(call["operation"]["trace"], call["inputs"][0]["trace"]);
    assert_ne!(call["operation"]["trace"], call["inputs"][1]["trace"]);
    let spans = exporter.get_finished_spans()?;
    let controls: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "control.apply")
        .collect();
    assert_eq!(controls.len(), 1);
    assert_eq!(controls[0].parent_span_id.to_bytes(), [93; 8]);
    let starts: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "component.start")
        .collect();
    assert_eq!(starts.len(), 2);
    assert!(starts
        .iter()
        .all(|span| span.parent_span_id == controls[0].span_context.span_id()));
    let batches: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "sink.handle_batch")
        .collect();
    if sampled {
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].links.links.len(), 2);
        assert_eq!(
            call["operation"]["span"],
            json!(batches[0].span_context.span_id().to_bytes())
        );
        assert!(observations["spans"]
            .as_array()
            .unwrap()
            .iter()
            .any(|span| {
                span["name"] == "fixture.sink" && span["parent"] == call["operation"]["span"]
            }));
    } else {
        assert!(batches.is_empty());
        assert!(!observations["spans"]
            .as_array()
            .unwrap()
            .iter()
            .any(|span| span["trace"] == call["operation"]["trace"]));
    }
    Ok(())
}

#[test]
fn actual_native_libraries_preserve_context_parentage_and_owned_cancellation() -> Result<()> {
    let plugin = load(false)?;
    assert!(plugin
        .factories()
        .iter()
        .all(|factory| factory.supports_execution_context()));
    for (multi_thread, seed, sampled) in [(false, 31, true), (true, 41, false)] {
        let mut builder = if multi_thread {
            tokio::runtime::Builder::new_multi_thread()
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        let runtime = builder.enable_all().build()?;
        runtime.block_on(pipeline(
            &plugin,
            if multi_thread {
                "native-two"
            } else {
                "native-one"
            },
            seed,
            sampled,
        ))?;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(batch_and_cancellation(&plugin))?;
    let observed = observations()?;
    let calls = observed["calls"].as_array().unwrap();
    for (instance, seed) in [("native-one", 31u8), ("native-two", 41u8)] {
        let middle = calls
            .iter()
            .find(|call| {
                call["phase"] == "transform" && call["instance"] == format!("graph-{instance}")
            })
            .context("scoped native transform")?;
        assert_eq!(middle["component"], MIDDLE_ID);
        assert_eq!(middle["graph"], format!("graph-{instance}"));
        assert!(middle["generation"].as_u64().unwrap() > 0);
        assert_eq!(middle["operation"], middle["inputs"][0]);
        assert_eq!(middle["current"]["trace"], json!(vec![seed; 16]));
    }
    let spans = observed["spans"].as_array().unwrap();
    let source = spans
        .iter()
        .find(|span| span["name"] == "fixture.source" && span["trace"] == json!(vec![31u8; 16]))
        .context("exported native source")?;
    let middle = spans
        .iter()
        .find(|span| span["name"] == "fixture.transform" && span["trace"] == json!(vec![31u8; 16]))
        .context("exported native transform")?;
    assert_eq!(middle["parent"], source["span"]);
    assert!(
        !spans
            .iter()
            .any(|span| span["trace"] == json!(vec![41u8; 16])),
        "unsampled trace was exported"
    );

    runtime.block_on(graph_created_native_batch(&plugin, true))?;
    runtime.block_on(graph_created_native_batch(&plugin, false))?;

    let base = load(true)?;
    assert!(base
        .factories()
        .iter()
        .all(|factory| !factory.supports_execution_context()));
    let strict = ExecutionScope::new(ExecutionContext::default()).with_required_transport(true);
    assert!(
        strict
            .in_scope(|| factory(&base, "transformer").create_component(
                ComponentId::try_new("2999619e-88eb-4909-bfc1-95d2e289c0b9").unwrap(),
                json!({}),
            ))
            .is_err(),
        "required native propagation must fail before construction even without a current trace"
    );
    runtime.block_on(async {
        let mut old = factory(&base, "transformer").create_component(
            ComponentId::try_new("c774822a-9723-46f6-8a50-f35947a3e416")?,
            json!({}),
        )?;
        old.initialize().await?;
        old.start().await?;
        let mut empty = input(61);
        empty.execution_context = ExecutionContext::default();
        assert!(old.transform(empty).await?[0].execution_context.is_empty());
        assert!(
            old.transform(input(62)).await.is_err(),
            "missing capability must not silently discard context"
        );
        old.stop().await?;
        old.dispose().await?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}
