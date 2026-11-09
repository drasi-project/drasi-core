// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! One source, continuous query, and handled application reaction per demo.
//! See ../README.md for the one-command dashboard and Jaeger setup.

mod dashboard;
mod scenarios;

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_core::computation::InMemoryComputationProvider;
use drasi_host_sdk::computation::NativePlugin;
use drasi_lib::{
    computation::v1::*,
    error::OperationFailures,
    observability::{
        ComponentLogRuntime, ComponentLogType, ExecutionContext, ExecutionScope, LogLevel,
        LogMessage, LogMetadata, LogPolicy, MetricRegistrationContext, NativeSpanCollector,
        OpenTelemetryContextAdapter, OpenTelemetrySpanProcessor, TelemetryAttribute,
        TelemetryOptions, TELEMETRY_TARGET,
    },
    DrasiLib,
};
use drasi_reaction_application::NativeApplicationReaction;
use metrics_exporter_prometheus::PrometheusBuilder;
use opentelemetry::trace::TracerProvider;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    trace::{BatchSpanProcessor, Sampler, SdkTracerProvider},
    Resource,
};
use serde_json::{json, Value};
use std::{env, num::NonZeroUsize, str::FromStr, sync::Arc, time::Duration};
use tracing_subscriber::{filter::filter_fn, prelude::*};

const INSTANCE: &str = "source-trace-demo";
const SERVICE: &str = "drasi-observability-demo";

fn setting(name: &str, default: &str) -> Result<String> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Ok(default.into()),
        Err(error) => Err(error.into()),
    }
}

fn port(name: &str, default: u16) -> Result<u16> {
    let value = match env::var(name) {
        Ok(value) => u16::from_str(&value).with_context(|| format!("{name} must be a TCP port"))?,
        Err(env::VarError::NotPresent) => default,
        Err(error) => return Err(error.into()),
    };
    ensure!(value != 0, "{name} must be nonzero");
    Ok(value)
}

fn id(name: &str) -> Result<ComponentId> {
    Ok(ComponentId::try_new(name)?)
}

fn endpoint(component: &str, name: &str) -> Result<Endpoint> {
    Ok(Endpoint::new(id(component)?, PortId::try_new(name)?))
}

fn stream(component: &str) -> Result<StreamId> {
    Ok(StreamId::try_new(format!("{component}/out"))?)
}

fn resource(name: &str) -> Result<ResourceId> {
    Ok(ResourceId::try_new(name)?)
}

struct PostgresSettings;

#[async_trait]
impl ConfigurationResolver for PostgresSettings {
    fn validate_reference(&self, key: &str) -> Result<()> {
        ensure!(key == "postgres", "unknown configuration reference");
        Ok(())
    }

    async fn resolve(&self, key: &str) -> Result<Value> {
        self.validate_reference(key)?;
        let password = env::var("PGPASSWORD").context("set PGPASSWORD for the demo database")?;
        ensure!(!password.is_empty(), "PGPASSWORD must be nonempty");
        Ok(json!({
            "connection": {
                "host": "127.0.0.1",
                "port": port("PGPORT", 15437)?,
                "database": "postgres",
                "user": "postgres",
                "password": password,
                "ssl_mode": "disable",
                "tables": ["trace_items"],
                "slot_name": "drasi_trace_demo",
                "publication_name": "drasi_trace_demo",
                "table_keys": [{"table": "trace_items", "key_columns": ["id"]}]
            },
            "start_lsn": "0/0",
            "transactions": {"max_changes": 64, "max_bytes": 1048576, "max_duration_ms": 10000},
            "max_protocol_bytes": 1048576,
            "io_timeout_ms": 10000,
            "feedback_interval_ms": 100
        }))
    }
}

fn finish(results: impl IntoIterator<Item = Result<()>>) -> Result<()> {
    let failures = results
        .into_iter()
        .filter_map(Result::err)
        .collect::<Vec<_>>();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(OperationFailures::new("observability demonstration failed", failures).into())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = scenarios::Config::from_env()?;
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(setting(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "http://127.0.0.1:14318/v1/traces",
        )?)
        .with_timeout(Duration::from_secs(3))
        .build()?;
    let processor = OpenTelemetrySpanProcessor::new(
        BatchSpanProcessor::builder(exporter).build(),
        Sampler::AlwaysOn,
    );
    let provider = SdkTracerProvider::builder()
        .with_sampler(processor.sampler())
        .with_span_processor(processor.clone())
        .with_resource(Resource::builder_empty().with_service_name(SERVICE).build())
        .build();
    let logging = ComponentLogRuntime::new(LogPolicy::new(&config.logs)?)?;
    let native_spans = NativeSpanCollector::new(Arc::new(processor), Default::default())?;
    let recorder = Arc::new(
        PrometheusBuilder::new()
            .set_buckets(&[0.00001, 0.0001, 0.001, 0.01, 0.1, 1.0, 10.0])?
            .build_recorder(),
    );
    let metrics = MetricRegistrationContext::new(recorder.clone(), Default::default())?;
    let settings = config.settings()?;
    let policy = settings.resolve()?;
    let result = async {
        tracing_subscriber::registry()
            .with(logging.layer())
            .with(
                tracing_opentelemetry::layer()
                    .with_tracer(provider.tracer("drasi-example"))
                    .with_filter(filter_fn(move |metadata| {
                        metadata.is_span()
                            && metadata.target() == TELEMETRY_TARGET
                            && policy.allows_span(metadata.name())
                    })),
            )
            .try_init()?;
        run(
            logging.clone(),
            native_spans.clone(),
            metrics,
            recorder.handle(),
            config,
        )
        .await
    }
    .await;
    let spans = native_spans
        .shutdown(Duration::from_secs(3))
        .await
        .map_err(Into::into);
    let logs = logging
        .shutdown(Duration::from_secs(3))
        .await
        .map_err(Into::into);
    let traces = tokio::task::spawn_blocking(move || provider.shutdown())
        .await
        .context("join trace exporter shutdown")
        .and_then(|result| result.map_err(Into::into));
    finish([result, spans, logs, traces])
}

async fn run(
    logging: ComponentLogRuntime,
    native_spans: NativeSpanCollector,
    metrics: MetricRegistrationContext,
    metric_handle: metrics_exporter_prometheus::PrometheusHandle,
    config: scenarios::Config,
) -> Result<()> {
    let settings = config.settings()?;
    let source = config.source();
    let source_plugin = NativePlugin::from_definition(if source == "postgres" {
        drasi_computation_source_postgres::plugin()?
    } else {
        drasi_computation_source_http::plugin()?
    })?;
    let source_factory = source_plugin
        .factories()
        .iter()
        .find(|factory| {
            source == "http"
                || factory.metadata().implementation.name.as_ref()
                    == drasi_computation_source_postgres::POSTGRES_ROWS
        })
        .context("demo source factory")?
        .clone();
    let http_port = if source == "http" {
        port("HTTP_PORT", 19090)?
    } else {
        19090
    };
    let lab = Arc::new(scenarios::Lab::new(
        config,
        metric_handle.clone(),
        http_port,
    )?);
    let mut batch = ComponentBatch::builder();
    let progress = if source == "postgres" {
        let progress = Arc::new(QuerySourceProgress::new(
            ComponentBatch::GRAPH_ID,
            id("postgres-query")?,
        )?);
        for (name, role, handle) in [
            (
                "postgres-settings",
                ResourceRole::SecretStore,
                ResourceHandle::new(
                    ResourceRole::SecretStore,
                    Arc::new(ConfigurationResolverResource(Arc::new(PostgresSettings))),
                ),
            ),
            (
                "postgres-progress",
                ResourceRole::Checkpoint,
                ResourceHandle::new(
                    ResourceRole::Checkpoint,
                    Arc::new(QuerySourceProgressResource(progress.clone())),
                ),
            ),
        ] {
            batch = batch
                .declare_resource(ResourceSpecification {
                    id: resource(name)?,
                    role,
                    ownership: ResourceOwnership::Borrowed,
                    binding: name.into(),
                })?
                .provide_resource(resource(name)?, handle)?;
        }
        let mut pg = source_factory.specification(
            id("postgres")?,
            json!({
                "stream": stream("postgres")?, "settings": {}, "persistent": false,
                "auto_create_slot": true, "start_from_slot": true, "reconnect_attempts": 0
            }),
        )?;
        pg.configuration.insert(
            "settings".into(),
            ConfigurationValue::Reference {
                resource: resource("postgres-settings")?,
                key: "postgres".into(),
                secret: true,
            },
        );
        pg.dependencies.insert(
            "source_progress".into(),
            vec![resource("postgres-progress")?],
        );
        batch = batch.component(pg, source_factory);
        Some(progress)
    } else {
        batch = batch.component(source_factory.specification(id("http")?, json!({
            "stream": stream("http")?, "sourceId": "http", "host": "127.0.0.1", "port": http_port
        }))?, source_factory);
        None
    };
    {
        let query_id = format!("{source}-query");
        let output_id = format!("{source}-output");
        let mut query = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id: ComponentBatch::GRAPH_ID.into(),
                id: id(&query_id)?,
                query: if source == "http" && lab.config.scenario == "latency" {
                    "MATCH (n:trace_items), (copy:demo_copies) RETURN n.id AS id, copy.id AS copy, n.name AS name, n.value AS value"
                } else {
                    "MATCH (n:trace_items) RETURN n.id AS id, n.name AS name, n.value AS value"
                }.into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: stream(&query_id)?,
                outbox_capacity: NonZeroUsize::new(32).context("query capacity")?,
            },
            Arc::new(InMemoryComputationProvider),
        )
        .await?
        .with_recovery_scope(INSTANCE)?;
        if let Some(progress) = progress {
            query = query.with_source_progress(progress.clone())?;
        }
        batch = batch
            .query(Box::new(query))
            .bind_stream(endpoint(source, "out")?, stream(source)?)
            .bind_stream(endpoint(&query_id, "out")?, stream(&query_id)?)
            .connect(
                EdgeDefinition::new(endpoint(source, "out")?, endpoint(&query_id, "in")?),
                Box::new(BoundedPipeConfig { capacity: 32 }),
            );
        let outputs = if source == "http" && lab.config.scenario == "latency" {
            vec!["http-fast".to_owned(), "http-slow".to_owned()]
        } else {
            vec![output_id]
        };
        for output in outputs {
            let observation = lab.clone();
            let callback_logging = logging.clone();
            let component = output.clone();
            let reaction = NativeApplicationReaction::callback(
                output.clone(),
                QueryChangeCodec::schema().descriptor().clone(),
                move |input| {
                    let observation = observation.clone();
                    let callback_logging = callback_logging.clone();
                    let component = component.clone();
                    async move {
                        observation
                            .before_handle(&component, component == "http-slow")
                            .await?;
                        let context = ExecutionContext::current();
                        let rows = input.envelope.changes().operations().len();
                        if let Some(trace) = context.trace() {
                            println!(
                                "RESULT source={source} query={source}-query component={component} trace_id={} span_id={} sampled={} changes={rows}",
                                opentelemetry::trace::TraceId::from_bytes(trace.trace_id()),
                                opentelemetry::trace::SpanId::from_bytes(trace.span_id()),
                                trace.sampled(),
                            );
                        } else {
                            ensure!(
                                !observation.config.tracing,
                                "handled callback lost enabled tracing context"
                            );
                            println!("RESULT source={source} query={source}-query component={component} trace_id=none sampled=false changes={rows}");
                        }
                        observation.handled(&component, rows, &context)?;
                        let (added, updated, deleted) =
                            input.envelope.changes().operations().iter().fold(
                                (0u64, 0u64, 0u64),
                                |(added, updated, deleted), operation| match operation {
                                    ChangeOperation::Added { .. } => (added + 1, updated, deleted),
                                    ChangeOperation::Updated { .. } => {
                                        (added, updated + 1, deleted)
                                    }
                                    ChangeOperation::Deleted { .. } => {
                                        (added, updated, deleted + 1)
                                    }
                                },
                            );
                        if tracing::enabled!(target: "drasi_demo", tracing::Level::INFO) {
                            let owner = ExecutionScope::current_component()
                                .context("handling log has no owning component scope")?;
                            let mut message = LogMessage::with_instance(
                                LogLevel::Info,
                                "Handled query result changes",
                                owner.identity.instance_id(),
                                owner
                                    .identity
                                    .component_id()
                                    .context("handling log has no component identity")?
                                    .as_str(),
                                ComponentLogType::Sink,
                            );
                            message.metadata = Some(LogMetadata {
                                graph_id: owner.identity.graph_id().into(),
                                plugin_kind: String::new(),
                                generation: owner.generation,
                                target: "drasi_demo".into(),
                                attributes: vec![
                                    (
                                        "query_id".into(),
                                        TelemetryAttribute::String(format!("{source}-query")),
                                    ),
                                    (
                                        "result_changes".into(),
                                        TelemetryAttribute::I64(i64::try_from(rows)?),
                                    ),
                                    (
                                        "added".into(),
                                        TelemetryAttribute::I64(i64::try_from(added)?),
                                    ),
                                    (
                                        "updated".into(),
                                        TelemetryAttribute::I64(i64::try_from(updated)?),
                                    ),
                                    (
                                        "deleted".into(),
                                        TelemetryAttribute::I64(i64::try_from(deleted)?),
                                    ),
                                ],
                                context: context.trace().cloned(),
                            });
                            // Capture loss is diagnosed by the log runtime, not a handling failure.
                            callback_logging.try_log(message);
                        }
                        Ok(())
                    }
                },
            )?;
            batch = batch.sink(Box::new(reaction)).connect(
                EdgeDefinition::new(endpoint(&query_id, "out")?, endpoint(&output, "in")?),
                Box::new(BoundedPipeConfig {
                    capacity: if lab.config.scenario == "latency" {
                        2
                    } else {
                        32
                    },
                }),
            );
        }
    }
    let core = DrasiLib::builder()
        .with_id(INSTANCE)
        .with_component_logging(logging.clone())
        .with_telemetry(
            TelemetryOptions::new(settings)
                .with_context_adapter(Arc::new(OpenTelemetryContextAdapter))
                .with_trace_directives("drasi::telemetry=trace,drasi_demo=info")
                .with_metric_context(metrics)
                .with_native_span_collector(native_spans.clone()),
        )
        .with_components(batch.build()?)
        .build()
        .await?;
    let dashboard_port = port("TELEMETRY_PORT", 19091)?;
    let mut dashboard = dashboard::Dashboard::start(
        dashboard_port,
        dashboard::DashboardState {
            metrics: metric_handle,
            logging: logging.clone(),
            spans: native_spans,
            lab: lab.clone(),
            jaeger: setting("JAEGER_URL", "http://127.0.0.1:16689")?,
            postgres_command: if source == "postgres" {
                Some(format!(
                    "{} exec -i {} psql -U postgres -v ON_ERROR_STOP=1 <<'SQL'\n\
                    INSERT INTO trace_items VALUES (2, 'SQL demo', 7);\n\
                    UPDATE trace_items SET value = 8 WHERE id = 2;\n\
                    DELETE FROM trace_items WHERE id = 2;\nSQL",
                    setting("CONTAINER_ENGINE", "podman")?,
                    setting(
                        "DEMO_POSTGRES_CONTAINER",
                        &format!("drasi-otel-demo-postgres-{}", port("PGPORT", 15437)?)
                    )?,
                ))
            } else {
                None
            },
        },
    )
    .await?;
    let mut dashboard_joined = false;
    let result = async {
        core.start().await?;
        if source == "http" {
            println!("READY HTTP http://127.0.0.1:{http_port}/sources/http/events");
        } else {
            println!("READY PostgreSQL publication/slot drasi_trace_demo; wait for its slot to be active before inserting");
        }
        println!("TRACES service={SERVICE}; each RESULT prints the trace ID to open in Jaeger");
        println!("DASHBOARD http://127.0.0.1:{dashboard_port}/ scenario={} sampling={}", lab.config.scenario, lab.config.sampling);
        println!("METRICS http://127.0.0.1:{dashboard_port}/metrics");
        println!("LOGS http://127.0.0.1:{dashboard_port}/logs");
        let control = core.computation_control()?;
        let mut observed = control.subscribe_observed();
        tokio::select! {
            stopped = tokio::signal::ctrl_c() => stopped.context("wait for Ctrl-C")?,
            server = &mut dashboard.task => {
                dashboard_joined = true;
                server.context("join telemetry dashboard")??;
                anyhow::bail!("telemetry dashboard stopped unexpectedly");
            }
            failed = observed.wait_for(|graph| graph.components.values().any(|component| component.failure.is_some())) => {
                let graph = failed.context("component observation closed")?;
                let failure = graph.components.values().find_map(|component| component.failure.as_ref())
                    .context("component failure disappeared")?;
                return Err(GraphError::Reported { cause: failure.cause.clone() }.into());
            }
        }
        Ok(())
    }.await;
    lab.close();
    let shutdown = core.shutdown().await.map_err(Into::into);
    let dashboard_shutdown = dashboard.shutdown(dashboard_joined).await;
    finish([result, shutdown, dashboard_shutdown])
}
