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
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

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
        assert_eq!(scope.identity.component_id().unwrap().as_str(), "collected");
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
            source.specification(
                ComponentId::try_new("source")?,
                json!({
                    "trace": true, "seed": seed, "sampled": sampled,
                }),
            )?,
            source,
        )
        .component(
            middle.specification(ComponentId::try_new("middle")?, json!({"trace": true}))?,
            middle,
        )
        .sink(Box::new(Collector {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("collected")?,
                vec![PortDescriptor::new(
                    PortId::try_new("in")?,
                    PortDirection::Input,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?,
            contexts: contexts.clone(),
            instance: format!("graph-{instance}"),
        }))
        .bind_stream(endpoint("source", "out"), StreamId::try_new("source/out")?)
        .bind_stream(endpoint("middle", "out"), StreamId::try_new("middle/out")?)
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("middle", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("middle", "out"), endpoint("collected", "in")),
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
            .create_component(ComponentId::try_new("batch")?, json!({"trace": true}))
    })?;
    sink.start().await?;
    scope
        .scope(sink.handle_batch(vec![input(52), input(53)]))
        .await?;
    sink.stop().await?;

    let mut middle = scope.in_scope(|| {
        factory(plugin, "transformer").create_component(
            ComponentId::try_new("cancelled")?,
            json!({"trace": true, "block": true}),
        )
    })?;
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
        assert_eq!(middle["component"], "middle");
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

    let base = load(true)?;
    assert!(base
        .factories()
        .iter()
        .all(|factory| !factory.supports_execution_context()));
    let strict = ExecutionScope::new(ExecutionContext::default()).with_required_transport(true);
    assert!(
        strict
            .in_scope(|| factory(&base, "transformer")
                .create_component(ComponentId::try_new("required").unwrap(), json!({})))
            .is_err(),
        "required native propagation must fail before construction even without a current trace"
    );
    runtime.block_on(async {
        let mut old = factory(&base, "transformer")
            .create_component(ComponentId::try_new("base")?, json!({}))?;
        old.start().await?;
        let mut empty = input(61);
        empty.execution_context = ExecutionContext::default();
        assert!(old.transform(empty).await?[0].execution_context.is_empty());
        assert!(
            old.transform(input(62)).await.is_err(),
            "missing capability must not silently discard context"
        );
        old.stop().await?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}
