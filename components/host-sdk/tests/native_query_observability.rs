// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_host_sdk::computation::{self, NativeComponentProxy, NativePlugin};
use drasi_lib::{computation::v1::*, observability::*};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[path = "../../../lib/tests/computation_support/metrics.rs"]
mod recording;
use recording::RecordingMetrics;

const QUERY_ID: &str = "c097b478-f23e-4b9d-9ec8-717b8daa1011";

fn library() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
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
    target.join("debug").join(format!(
        "{}drasi_computation_transformer_query{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX,
    ))
}

fn plugin() -> Result<Arc<NativePlugin>> {
    let path = library();
    anyhow::ensure!(
        path.is_file(),
        "build the matched native query plugin before running this test: {}",
        path.display()
    );
    computation::load(path)
}

fn input(sequence: u64, labels: &[&str], count: usize) -> Result<InputEnvelope> {
    let changes = (0..count)
        .map(|row| SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", &format!("{sequence}-{row}")),
                    labels: labels
                        .iter()
                        .map(|label| Arc::from(*label))
                        .collect::<Vec<_>>()
                        .into(),
                    effective_from: sequence,
                },
                properties: ElementPropertyMap::from(serde_json::json!({"value": row})),
            },
        })
        .collect::<Vec<_>>();
    Ok(InputEnvelope::new(
        PortId::try_new("in")?,
        GraphChangeCodec::encode_changes(
            &changes,
            StreamId::try_new("source/out")?,
            sequence,
            None,
        )?,
    ))
}

fn graph(
    metrics: &RecordingMetrics,
    instance: &str,
    profile: TelemetryProfile,
) -> Result<ComputationGraph> {
    Ok(ComputationGraph::builder("query-observations")
        .with_telemetry(
            TelemetryOptions::new(
                TelemetrySettings::new(profile)
                    .with_tracing(false)
                    .with_sampling(RootSampling::Never),
            )
            .with_instance_id(instance.to_owned())
            .with_metric_context(MetricRegistrationContext::new(
                Arc::new(metrics.clone()),
                Default::default(),
            )?),
        )
        .build()?)
}

fn binding(graph: &ComputationGraph, instance: &str) -> Result<ExecutionScope> {
    let scope = TelemetryScope::graph(instance, "query-observations")?.component(
        ComponentId::try_new(QUERY_ID)?,
        ComponentRole::Transformer,
        ComponentSemanticKind::Query,
    );
    let telemetry = graph
        .telemetry()
        .plugin_binding(scope.clone(), 1, "query", None)?;
    Ok(ExecutionScope::new(ExecutionContext::default())
        .with_component(scope, 1)
        .with_plugin_telemetry(Some(telemetry)))
}

fn create(
    plugin: &NativePlugin,
    execution: &ExecutionScope,
    instance: &str,
    provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>,
    volatile: bool,
) -> Result<NativeComponentProxy> {
    create_with_text(
        plugin,
        execution,
        instance,
        provider,
        volatile,
        "MATCH (n:Item) RETURN n.value AS value",
    )
}

fn create_with_text(
    plugin: &NativePlugin,
    execution: &ExecutionScope,
    instance: &str,
    provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>,
    volatile: bool,
    text: &str,
) -> Result<NativeComponentProxy> {
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/indexed-query"
        })
        .context("indexed query factory was not exported")?;
    execution.in_scope(|| {
        factory.create_component_with_processing(
            ComponentId::try_new(QUERY_ID)?,
            serde_json::json!({"query": text, "volatile": volatile}),
            drasi_computation_plugin_sdk::Scope {
                instance_id: instance.into(),
                graph_id: "query-observations".into(),
                generation: 1,
            },
            provider,
        )
    })
}

async fn counts_and_idle_work(instance: &str, profile: TelemetryProfile) -> Result<()> {
    let plugin = plugin()?;
    let metrics = RecordingMetrics::default();
    let graph = graph(&metrics, instance, profile)?;
    let execution = binding(&graph, instance)?;
    let mut query = create(
        &plugin,
        &execution,
        instance,
        Arc::new(InMemoryComputationProvider),
        true,
    )?;
    query.initialize().await?;
    query.start().await?;
    assert!(query.on_wakeup().await?.is_empty());
    assert_eq!(metrics.count("drasi.query.events_processed", &[]), 0);
    assert!(metrics
        .histogram("drasi.query.engine.duration", &[])
        .is_empty());
    assert!(metrics
        .histogram("drasi.query.commit.duration", &[])
        .is_empty());

    let first = input(1, &["Item"], 2)?;
    let output = execution.scope(query.transform(first.clone())).await?;
    assert_eq!(output.len(), 1);
    assert_eq!(
        QueryChangeCodec::decode_evaluation(&output[0].envelope)?.len(),
        2
    );
    let enabled = profile != TelemetryProfile::Off;
    assert_eq!(
        metrics.count("drasi.query.events_processed", &[]),
        u64::from(enabled)
    );
    assert_eq!(
        metrics.count("drasi.query.results_emitted", &[]),
        2 * u64::from(enabled)
    );
    assert_eq!(
        metrics.histogram("drasi.query.engine.duration", &[]).len(),
        usize::from(enabled)
    );
    assert_eq!(
        metrics.histogram("drasi.query.commit.duration", &[]).len(),
        usize::from(enabled)
    );
    query.delivery_completed(&output).await?;
    let engine = metrics.histogram("drasi.query.engine.duration", &[]).len();
    let commits = metrics.histogram("drasi.query.commit.duration", &[]).len();

    assert!(execution.scope(query.transform(first)).await?.is_empty());
    assert!(query.on_wakeup().await?.is_empty());
    assert_eq!(
        metrics.histogram("drasi.query.engine.duration", &[]).len(),
        engine
    );
    assert_eq!(
        metrics.histogram("drasi.query.commit.duration", &[]).len(),
        commits
    );
    assert!(execution
        .scope(query.transform(input(2, &["Other"], 1)?))
        .await?
        .is_empty());
    assert_eq!(
        metrics.count("drasi.query.events_processed", &[]),
        2 * u64::from(enabled)
    );
    assert_eq!(
        metrics.count("drasi.query.results_emitted", &[]),
        2 * u64::from(enabled)
    );
    assert_eq!(
        metrics.histogram("drasi.query.engine.duration", &[]).len(),
        2 * usize::from(enabled)
    );
    query.stop().await?;
    query.dispose().await?;
    if !enabled {
        assert!(metrics.is_empty());
    }
    drop(query);
    drop(graph);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn real_query_counts_inputs_and_rows_without_traces_current_thread() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(20),
        counts_and_idle_work("counts-current", TelemetryProfile::Persistence),
    )
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_query_counts_inputs_and_rows_without_traces_multi_thread() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(20),
        counts_and_idle_work("counts-multi", TelemetryProfile::Persistence),
    )
    .await?
}

#[tokio::test(flavor = "current_thread")]
async fn off_has_no_query_registrations() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(20),
        counts_and_idle_work("counts-off", TelemetryProfile::Off),
    )
    .await?
}

#[tokio::test(flavor = "current_thread")]
async fn standalone_volatile_evaluation_does_not_invent_storage_commit() -> Result<()> {
    let plugin = plugin()?;
    let metrics = RecordingMetrics::default();
    let graph = graph(&metrics, "standalone-volatile", TelemetryProfile::Debug)?;
    let execution = binding(&graph, "standalone-volatile")?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/volatile-query"
        })
        .context("volatile query factory")?;
    let mut query = execution.in_scope(|| {
        factory.create_component(
        ComponentId::try_new(QUERY_ID)?,
        serde_json::json!({"query": "MATCH (n:Item) RETURN n.value AS value", "volatile": true}),
    )
    })?;
    query.initialize().await?;
    query.start().await?;
    let output = execution
        .scope(query.transform(input(1, &["Item"], 1)?))
        .await?;
    assert_eq!(output.len(), 1);
    assert_eq!(metrics.count("drasi.query.events_processed", &[]), 1);
    assert_eq!(
        metrics.histogram("drasi.query.engine.duration", &[]).len(),
        1
    );
    assert!(metrics
        .histogram("drasi.query.commit.duration", &[])
        .is_empty());
    query.delivery_completed(&output).await?;
    query.stop().await?;
    query.dispose().await?;
    Ok(())
}

struct Bootstrap(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl ComputationBootstrapProvider for Bootstrap {
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures_util::stream::iter((1..=3).map(|sequence| {
                input(sequence, &["Item"], 1).map(|input| input.envelope)
            }))),
            watermarks: Vec::new(),
        })
    }
}

async fn bootstrap_metrics(instance: &str) -> Result<()> {
    let plugin = plugin()?;
    let metrics = RecordingMetrics::default();
    let graph = graph(&metrics, instance, TelemetryProfile::Debug)?;
    let execution = binding(&graph, instance)?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/indexed-query"
        })
        .context("indexed query factory")?;
    let mut query = execution.in_scope(|| factory.create_component_with_processing(
        ComponentId::try_new(QUERY_ID)?,
        serde_json::json!({"query": "MATCH (n:Item) RETURN n.value AS value", "volatile": true, "bootstrap": true}),
        drasi_computation_plugin_sdk::Scope { instance_id: instance.into(), graph_id: "query-observations".into(), generation: 1 },
        Arc::new(InMemoryComputationProvider),
    ))?;
    let bootstrap = Arc::new(Bootstrap(std::sync::atomic::AtomicUsize::new(0)));
    query.bind_bootstrap(Arc::new(computation::NativeBootstrapResource(
        bootstrap.clone(),
    )))?;
    query.initialize().await?;
    execution.scope(query.start()).await?;
    assert_eq!(
        metrics.count("drasi.query.events_processed", &[("phase", "bootstrap")]),
        3
    );
    assert_eq!(
        metrics.count("drasi.query.events_processed", &[("phase", "steady")]),
        0
    );
    assert_eq!(
        metrics
            .histogram("drasi.query.engine.duration", &[("phase", "bootstrap")])
            .len(),
        3
    );
    assert_eq!(metrics.count("drasi.query.results_emitted", &[]), 0);
    query.stop().await?;
    execution.scope(query.start()).await?;
    assert_eq!(
        bootstrap.0.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "healthy restart must not bootstrap again"
    );
    assert_eq!(
        metrics.count("drasi.query.events_processed", &[("phase", "bootstrap")]),
        3
    );
    let output = execution
        .scope(query.transform(input(4, &["Item"], 1)?))
        .await?;
    assert_eq!(output.len(), 1);
    assert_eq!(
        metrics.count("drasi.query.events_processed", &[("phase", "steady")]),
        1
    );
    assert_eq!(metrics.count("drasi.query.results_emitted", &[]), 1);
    query.delivery_completed(&output).await?;
    query.stop().await?;
    query.dispose().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_and_reused_startup_keep_distinct_counts_current_thread() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(20),
        bootstrap_metrics("bootstrap-current"),
    )
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_and_reused_startup_keep_distinct_counts_multi_thread() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(20),
        bootstrap_metrics("bootstrap-multi"),
    )
    .await?
}

async fn real_scheduled_work(instance: &str) -> Result<()> {
    let plugin = plugin()?;
    let metrics = RecordingMetrics::default();
    let graph = graph(&metrics, instance, TelemetryProfile::Debug)?;
    let execution = binding(&graph, instance)?;
    let mut query = create_with_text(
        &plugin, &execution, instance, Arc::new(InMemoryComputationProvider), true,
        "MATCH (n:Item) WHERE drasi.trueFor(n.ready, duration({seconds: 1})) RETURN n.value AS value",
    )?;
    query.initialize().await?;
    query.start().await?;
    assert!(query.on_wakeup().await?.is_empty());
    let change = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", "scheduled"),
                labels: Arc::from([Arc::from("Item")]),
                effective_from: 1_000,
            },
            properties: ElementPropertyMap::from(serde_json::json!({"ready": true, "value": 7})),
        },
    };
    let envelope = GraphChangeCodec::encode_change(
        change,
        StreamId::try_new("source/out")?,
        1,
        Some(chrono::DateTime::from_timestamp_millis(1_000).context("valid logical time")?),
    )?;
    assert!(execution
        .scope(query.transform(InputEnvelope::new(PortId::try_new("in")?, envelope)))
        .await?
        .is_empty());
    assert_eq!(metrics.count("drasi.query.events_processed", &[]), 1);
    assert_eq!(
        metrics.histogram("drasi.query.engine.duration", &[]).len(),
        1
    );
    let output = query.on_wakeup().await?;
    assert_eq!(output.len(), 1);
    assert_eq!(
        metrics.histogram("drasi.query.engine.duration", &[]).len(),
        2
    );
    assert_eq!(metrics.count("drasi.query.events_processed", &[]), 1);
    assert_eq!(metrics.count("drasi.query.results_emitted", &[]), 1);
    query.delivery_completed(&output).await?;
    let commits = metrics.histogram("drasi.query.commit.duration", &[]).len();
    assert!(query.on_wakeup().await?.is_empty());
    assert_eq!(
        metrics.histogram("drasi.query.engine.duration", &[]).len(),
        2
    );
    assert_eq!(
        metrics.histogram("drasi.query.commit.duration", &[]).len(),
        commits
    );
    query.stop().await?;
    query.dispose().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn scheduled_work_is_distinct_from_input_and_empty_probes_current_thread() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(20),
        real_scheduled_work("scheduled-current"),
    )
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_work_is_distinct_from_input_and_empty_probes_multi_thread() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(20),
        real_scheduled_work("scheduled-multi"),
    )
    .await?
}

async fn replay_does_not_evaluate(instance: &str) -> Result<()> {
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    let directory = tempfile::tempdir()?;
    let provider = Arc::new(RocksDbComputationProvider::new(
        directory.path(),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
        ),
    ));
    let plugin = plugin()?;
    let metrics = RecordingMetrics::default();
    let graph = graph(&metrics, instance, TelemetryProfile::Debug)?;
    let execution = binding(&graph, instance)?;
    let mut query = create(&plugin, &execution, instance, provider, false)?;
    query.initialize().await?;
    query.start().await?;
    let original = execution
        .scope(query.transform(input(1, &["Item"], 1)?))
        .await?;
    assert_eq!(original.len(), 1);
    query.stop().await?;
    query.start().await?;
    let evaluations = metrics.histogram("drasi.query.engine.duration", &[]).len();
    let inputs = metrics.count("drasi.query.events_processed", &[]);
    let results = metrics.count("drasi.query.results_emitted", &[]);
    assert!(query.has_pending_emissions());
    let replay = query.continue_transform().await?;
    assert_eq!(replay.len(), 1);
    assert_eq!(
        QueryChangeCodec::decode_evaluation(&replay[0].envelope)?,
        QueryChangeCodec::decode_evaluation(&original[0].envelope)?
    );
    query.delivery_completed(&replay).await?;
    assert_eq!(
        metrics.histogram("drasi.query.engine.duration", &[]).len(),
        evaluations
    );
    assert_eq!(metrics.count("drasi.query.events_processed", &[]), inputs);
    assert_eq!(metrics.count("drasi.query.results_emitted", &[]), results);
    assert_eq!(inputs, 1);
    assert_eq!(results, 1);
    query.stop().await?;
    query.dispose().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn retained_output_replay_does_not_count_fresh_work_current_thread() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(20),
        replay_does_not_evaluate("replay-current"),
    )
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_output_replay_does_not_count_fresh_work_multi_thread() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(20),
        replay_does_not_evaluate("replay-multi"),
    )
    .await?
}

#[tokio::test(flavor = "current_thread")]
async fn non_atomic_evaluation_and_publication_keep_one_input_and_result_count() -> Result<()> {
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    let directory = tempfile::tempdir()?;
    let provider = Arc::new(RocksDbComputationProvider::new(
        directory.path(),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
        ),
    ));
    let plugin = plugin()?;
    let metrics = RecordingMetrics::default();
    let graph = graph(&metrics, "non-atomic", TelemetryProfile::Persistence)?;
    let execution = binding(&graph, "non-atomic")?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/indexed-query"
        })
        .context("indexed query factory")?;
    let mut query = execution.in_scope(|| factory.create_component_with_processing_publication(
        ComponentId::try_new(QUERY_ID)?,
        serde_json::json!({"query": "MATCH (n:Item) RETURN n.value AS value", "publication": "non_atomic"}),
        drasi_computation_plugin_sdk::Scope { instance_id: "non-atomic".into(), graph_id: "query-observations".into(), generation: 1 },
        provider, None, QueryPublicationMode::NonAtomic,
    ))?;
    query.initialize().await?;
    query.start().await?;
    let original = input(1, &["Item"], 2)?;
    let output = execution.scope(query.transform(original.clone())).await?;
    assert_eq!(output.len(), 1);
    assert_eq!(metrics.count("drasi.query.events_processed", &[]), 1);
    assert_eq!(metrics.count("drasi.query.results_emitted", &[]), 2);
    assert_eq!(
        metrics.histogram("drasi.query.engine.duration", &[]).len(),
        1
    );
    assert_eq!(
        metrics.histogram("drasi.query.commit.duration", &[]).len(),
        2
    );
    query.delivery_completed(&output).await?;
    let commits = metrics.histogram("drasi.query.commit.duration", &[]).len();
    assert!(execution.scope(query.transform(original)).await?.is_empty());
    assert_eq!(metrics.count("drasi.query.events_processed", &[]), 1);
    assert_eq!(metrics.count("drasi.query.results_emitted", &[]), 2);
    assert_eq!(
        metrics.histogram("drasi.query.engine.duration", &[]).len(),
        1
    );
    assert_eq!(
        metrics.histogram("drasi.query.commit.duration", &[]).len(),
        commits
    );
    query.stop().await?;
    query.dispose().await?;
    Ok(())
}

#[cfg(feature = "otel-context")]
mod traces {
    use super::*;
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_sdk::trace::{
        InMemorySpanExporter, Sampler, SdkTracerProvider, SimpleSpanProcessor,
    };
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::prelude::*;

    #[tokio::test(flavor = "current_thread")]
    async fn loaded_query_bootstrap_has_one_linked_root_without_per_row_spans() -> Result<()> {
        let metrics = RecordingMetrics::default();
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("query-bootstrap")));
        async {
            let graph = ComputationGraph::builder("query-observations")
                .with_telemetry(
                    TelemetryOptions::new(TelemetryProfile::Debug)
                        .with_instance_id("bootstrap-traces")
                        .with_metric_context(MetricRegistrationContext::new(Arc::new(metrics.clone()), Default::default())?)
                        .with_context_adapter(Arc::new(OpenTelemetryContextAdapter)),
                ).build()?;
            let execution = binding(&graph, "bootstrap-traces")?
                .with_adapter(Some(Arc::new(OpenTelemetryContextAdapter)))
                .with_context(ExecutionContext::new(TraceContextV1::new([41; 16], [42; 8], 1, None)?));
            let plugin = plugin()?;
            let factory = plugin.factories().iter()
                .find(|factory| factory.metadata().implementation.name.as_ref() == "drasi.experimental/indexed-query")
                .context("indexed query factory")?;
            let mut query = execution.in_scope(|| factory.create_component_with_processing(
                ComponentId::try_new(QUERY_ID)?,
                serde_json::json!({"query": "MATCH (n:Item) RETURN n.value AS value", "volatile": true, "bootstrap": true}),
                drasi_computation_plugin_sdk::Scope { instance_id: "bootstrap-traces".into(), graph_id: "query-observations".into(), generation: 1 },
                Arc::new(InMemoryComputationProvider),
            ))?;
            query.bind_bootstrap(Arc::new(computation::NativeBootstrapResource(Arc::new(
                Bootstrap(std::sync::atomic::AtomicUsize::new(0)),
            ))))?;
            query.initialize().await?;
            execution.scope(query.start()).await?;
            query.stop().await?;
            query.dispose().await?;
            Ok::<_, anyhow::Error>(())
        }.with_subscriber(subscriber).await?;
        provider.force_flush()?;
        let spans = exporter.get_finished_spans()?;
        let roots = spans
            .iter()
            .filter(|span| span.name == "query.bootstrap")
            .collect::<Vec<_>>();
        assert_eq!(roots.len(), 1);
        assert_eq!(
            roots[0].parent_span_id,
            opentelemetry::trace::SpanId::INVALID
        );
        assert_eq!(roots[0].links.links.len(), 1);
        assert_eq!(
            roots[0].links.links[0].span_context.trace_id().to_bytes(),
            [41; 16]
        );
        assert_eq!(
            roots[0].links.links[0].span_context.span_id().to_bytes(),
            [42; 8]
        );
        assert!(
            roots[0]
                .attributes
                .contains(&opentelemetry::KeyValue::new("input_count", 3_i64)),
            "bootstrap root attributes: {:?}",
            roots[0].attributes
        );
        assert!(!spans.iter().any(|span| matches!(
            span.name.as_ref(),
            "query.evaluate"
                | "query.commit"
                | "query.prepare_input"
                | "query.prepare_output"
                | "query.finalize"
        )));
        assert_eq!(
            metrics
                .histogram("drasi.query.engine.duration", &[("phase", "bootstrap")])
                .len(),
            3
        );
        provider.shutdown()?;
        Ok(())
    }

    struct InputSource(ComponentDescriptor, Option<OutputEnvelope>);
    #[async_trait::async_trait]
    impl ComputationComponent for InputSource {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.0
        }
        async fn start(&mut self) -> Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }
    #[async_trait::async_trait]
    impl EnvelopeSource for InputSource {
        async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
            Ok(self.1.take())
        }
    }
    struct OutputSink(ComponentDescriptor, Arc<tokio::sync::Notify>);
    #[async_trait::async_trait]
    impl ComputationComponent for OutputSink {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.0
        }
        async fn start(&mut self) -> Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }
    #[async_trait::async_trait]
    impl EnvelopeSink for OutputSink {
        fn completion(&self) -> SinkCompletion {
            SinkCompletion::Handled
        }
        async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
            assert_eq!(
                QueryChangeCodec::decode_evaluation(&input.envelope)?.len(),
                2
            );
            self.1.notify_one();
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn graph_and_loaded_query_share_one_processing_observation() -> Result<()> {
        use std::collections::BTreeMap;
        let metrics = RecordingMetrics::default();
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("graph-query")));
        async {
            let plugin = plugin()?;
            let factory = plugin
                .factories()
                .iter()
                .find(|factory| {
                    factory.metadata().implementation.name.as_ref()
                        == "drasi.experimental/indexed-query"
                })
                .context("indexed query factory")?
                .clone();
            let query = ComponentId::try_new(QUERY_ID)?;
            let source = ComponentId::generate();
            let sink = ComponentId::generate();
            let resource = ResourceId::try_new("processing")?;
            let mut specification = factory.specification(
                query.clone(),
                serde_json::json!({
                    "query": "MATCH (n:Item) RETURN n.value AS value", "volatile": true, "stream": "query/out",
                }),
            )?;
            specification.dependencies =
                BTreeMap::from([(Arc::from("processing"), vec![resource.clone()])]);
            let done = Arc::new(tokio::sync::Notify::new());
            let port = |name, direction, schema| -> Result<_> {
                Ok(PortDescriptor::new(
                    PortId::try_new(name)?,
                    direction,
                    schema,
                    PipeRequirements::default(),
                ))
            };
            let mut graph = ComputationGraph::builder("query-graph")
                .with_telemetry(
                    TelemetryOptions::new(TelemetryProfile::Debug)
                        .with_instance_id("real-query-graph")
                        .with_metric_context(MetricRegistrationContext::new(
                            Arc::new(metrics.clone()),
                            Default::default(),
                        )?)
                        .with_context_adapter(Arc::new(OpenTelemetryContextAdapter)),
                )
                .declare_resource(ResourceSpecification {
                    id: resource.clone(),
                    role: ResourceRole::StateStore,
                    ownership: ResourceOwnership::Borrowed,
                    binding: "processing".into(),
                })?
                .provide_resource(
                    resource,
                    ResourceHandle::new(
                        ResourceRole::StateStore,
                        Arc::new(computation::NativeProcessingResource::new(Arc::new(
                            InMemoryComputationProvider,
                        ))),
                    ),
                )?
                .source(
                    ComponentName::try_new("source")?,
                    Box::new(InputSource(
                        ComponentDescriptor::try_new(
                            source.clone(),
                            vec![port(
                                "out",
                                PortDirection::Output,
                                GraphChangeCodec::schema().descriptor().clone(),
                            )?],
                        )?,
                        Some(OutputEnvelope::new(
                            PortId::try_new("out")?,
                            input(1, &["Item"], 2)?.envelope,
                        )),
                    )),
                )
                .component(ComponentName::try_new("query")?, specification, factory)
                .sink(
                    ComponentName::try_new("sink")?,
                    Box::new(OutputSink(
                        ComponentDescriptor::try_new(
                            sink.clone(),
                            vec![port(
                                "in",
                                PortDirection::Input,
                                QueryChangeCodec::schema().descriptor().clone(),
                            )?],
                        )?,
                        done.clone(),
                    )),
                )
                .bind_stream(
                    Endpoint::new(source.clone(), PortId::try_new("out")?),
                    StreamId::try_new("source/out")?,
                )
                .bind_stream(
                    Endpoint::new(query.clone(), PortId::try_new("out")?),
                    StreamId::try_new("query/out")?,
                )
                .connect(
                    EdgeDefinition::new(
                        Endpoint::new(source, PortId::try_new("out")?),
                        Endpoint::new(query.clone(), PortId::try_new("in")?),
                    ),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                )
                .connect(
                    EdgeDefinition::new(
                        Endpoint::new(query, PortId::try_new("out")?),
                        Endpoint::new(sink, PortId::try_new("in")?),
                    ),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                )
                .build()?;
            let run = graph.start()?;
            let control = run.control();
            let (result, ()) = tokio::time::timeout(Duration::from_secs(20), async {
                tokio::join!(run, async {
                    done.notified().await;
                    control.cancel();
                })
            })
            .await?;
            assert!(matches!(result, Ok(()) | Err(GraphError::Cancelled)));
            graph.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        }
        .with_subscriber(subscriber)
        .await?;
        provider.force_flush()?;
        let spans = exporter.get_finished_spans()?;
        let processing = spans
            .iter()
            .filter(|span| span.name == "query.process")
            .collect::<Vec<_>>();
        assert_eq!(processing.len(), 1);
        let evaluations = spans
            .iter()
            .filter(|span| span.name == "query.evaluate")
            .collect::<Vec<_>>();
        assert_eq!(evaluations.len(), 1);
        assert_eq!(
            evaluations[0].parent_span_id,
            processing[0].span_context.span_id()
        );
        assert_eq!(metrics.count("drasi.query.events_processed", &[]), 1);
        assert_eq!(metrics.count("drasi.query.results_emitted", &[]), 2);
        provider.shutdown()?;
        Ok(())
    }

    async fn phase_parentage(instance: &str, sampled: bool) -> Result<()> {
        let exporter = InMemorySpanExporter::default();
        let processor = OpenTelemetrySpanProcessor::new(
            SimpleSpanProcessor::new(exporter.clone()),
            Sampler::AlwaysOn,
        );
        let provider = SdkTracerProvider::builder()
            .with_sampler(processor.sampler())
            .with_span_processor(processor.clone())
            .build();
        let collector = NativeSpanCollector::new(Arc::new(processor), Default::default())?;
        let metrics = RecordingMetrics::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(provider.tracer("native-query-tests")),
        );
        let result = async {
            let graph = ComputationGraph::builder("query-observations")
                .with_telemetry(
                    TelemetryOptions::new(TelemetryProfile::Debug)
                        .with_instance_id(instance.to_owned())
                        .with_metric_context(MetricRegistrationContext::new(
                            Arc::new(metrics.clone()),
                            Default::default(),
                        )?)
                        .with_context_adapter(Arc::new(OpenTelemetryContextAdapter))
                        .with_native_span_collector(collector.clone()),
                )
                .build()?;
            let execution = binding(&graph, instance)?
                .with_adapter(Some(Arc::new(OpenTelemetryContextAdapter)));
            let plugin = plugin()?;
            let mut query = create(
                &plugin,
                &execution,
                instance,
                Arc::new(InMemoryComputationProvider),
                true,
            )?;
            query.initialize().await?;
            query.start().await?;
            let parent = TraceContextV1::new([1; 16], [2; 8], u8::from(sampled), None)?;
            let input = input(1, &["Item"], 1)?
                .with_execution_context(ExecutionContext::new(parent.clone()));
            let output = execution
                .with_context(ExecutionContext::new(parent.clone()))
                .scope(query.transform(input))
                .await?;
            assert_eq!(output.len(), 1);
            assert_eq!(output[0].execution_context.trace(), Some(&parent));
            collector.flush(Duration::from_secs(3)).await?;
            provider.force_flush()?;
            let spans = exporter.get_finished_spans()?;
            for name in [
                "query.evaluate",
                "query.commit",
                "query.prepare_input",
                "query.prepare_output",
                "query.finalize",
            ] {
                let values = spans
                    .iter()
                    .filter(|span| span.name == name)
                    .collect::<Vec<_>>();
                assert_eq!(values.len(), usize::from(sampled), "{name}");
                for span in values {
                    assert_eq!(
                        span.span_context.trace_id().to_bytes(),
                        parent.trace_id(),
                        "{name}"
                    );
                    assert_eq!(span.parent_span_id.to_bytes(), parent.span_id(), "{name}");
                }
            }
            assert!(!format!("{spans:?}").contains("MATCH (n:Item)"));
            assert_eq!(
                metrics.histogram("drasi.query.engine.duration", &[]).len(),
                1
            );
            assert!(
                metrics
                    .histogram("drasi.query.commit.duration", &[])
                    .is_empty(),
                "Debug tracing must not enable Persistence-only commit metrics"
            );
            query.delivery_completed(&output).await?;
            query.stop().await?;
            query.dispose().await?;
            drop(query);
            drop(graph);
            Ok::<_, anyhow::Error>(())
        }
        .with_subscriber(subscriber)
        .await;
        collector.shutdown(Duration::from_secs(3)).await?;
        provider.shutdown()?;
        result
    }

    #[tokio::test(flavor = "current_thread")]
    async fn real_plugin_evaluation_commit_and_phases_preserve_parentage() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(20), phase_parentage("parentage", true)).await?
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unsampled_real_query_keeps_metrics_without_phase_spans() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(20), phase_parentage("unsampled", false)).await?
    }
}
