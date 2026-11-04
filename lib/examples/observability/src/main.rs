// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Real HTTP and PostgreSQL sources, continuous queries and handled callbacks.
//! See ../README.md for the local PostgreSQL and Jaeger setup.

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_core::computation::InMemoryComputationProvider;
use drasi_host_sdk::computation::NativePlugin;
use drasi_lib::{
    computation::v1::*,
    error::OperationFailures,
    observability::{
        ComponentLogRuntime, ExecutionContext, LogPolicy, OpenTelemetryContextAdapter,
        TelemetryOptions, TelemetryProfile, TelemetrySampler, TelemetrySettings, TELEMETRY_TARGET,
    },
    DrasiLib,
};
use drasi_reaction_application::NativeApplicationReaction;
use opentelemetry::trace::TracerProvider;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    trace::{Sampler, SdkTracerProvider},
    Resource,
};
use serde_json::{json, Value};
use std::{env, num::NonZeroUsize, str::FromStr, sync::Arc, time::Duration};
use tracing_subscriber::{filter::filter_fn, prelude::*};

const INSTANCE: &str = "source-trace-demo";
const SERVICE: &str = "drasi-source-traces";

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
        Err(OperationFailures::new("source trace demonstration failed", failures).into())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(setting(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "http://127.0.0.1:14318/v1/traces",
        )?)
        .with_timeout(Duration::from_secs(3))
        .build()?;
    let provider = SdkTracerProvider::builder()
        .with_sampler(TelemetrySampler(Sampler::AlwaysOn))
        .with_batch_exporter(exporter)
        .with_resource(Resource::builder_empty().with_service_name(SERVICE).build())
        .build();
    let logging = ComponentLogRuntime::new(LogPolicy::new("info")?)?;
    let settings = TelemetrySettings::new(TelemetryProfile::Debug)
        .with_metrics(false)
        .require_distributed_tracing();
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
        run(logging.clone(), settings).await
    }
    .await;
    let logs = logging
        .shutdown(Duration::from_secs(3))
        .await
        .map_err(Into::into);
    let traces = tokio::task::spawn_blocking(move || provider.shutdown())
        .await
        .context("join trace exporter shutdown")
        .and_then(|result| result.map_err(Into::into));
    finish([result, logs, traces])
}

async fn run(logging: ComponentLogRuntime, settings: TelemetrySettings) -> Result<()> {
    let http = NativePlugin::from_definition(drasi_computation_source_http::plugin()?)?;
    let postgres = NativePlugin::from_definition(drasi_computation_source_postgres::plugin()?)?;
    let http_factory = http
        .factories()
        .first()
        .context("HTTP source factory")?
        .clone();
    let postgres_factory = postgres
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == drasi_computation_source_postgres::POSTGRES_ROWS
        })
        .context("PostgreSQL row source factory")?
        .clone();
    let progress = Arc::new(QuerySourceProgress::new(
        ComponentBatch::GRAPH_ID,
        id("postgres-query")?,
    )?);
    let mut pg = postgres_factory.specification(
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
    let http_port = port("HTTP_PORT", 19090)?;
    let mut batch = ComponentBatch::builder();
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
    batch = batch
        .component(http_factory.specification(id("http")?, json!({
            "stream": stream("http")?, "sourceId": "http", "host": "127.0.0.1", "port": http_port
        }))?, http_factory)
        .component(pg, postgres_factory);
    for source in ["http", "postgres"] {
        let query_id = format!("{source}-query");
        let output_id = format!("{source}-output");
        let mut query = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id: ComponentBatch::GRAPH_ID.into(),
                id: id(&query_id)?,
                query: "MATCH (n:trace_items) RETURN n.id AS id, n.name AS name, n.value AS value"
                    .into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: stream(&query_id)?,
                outbox_capacity: NonZeroUsize::new(32).context("query capacity")?,
            },
            Arc::new(InMemoryComputationProvider),
        )
        .await?
        .with_recovery_scope(INSTANCE)?;
        if source == "postgres" {
            query = query.with_source_progress(progress.clone())?;
        }
        let reaction = NativeApplicationReaction::callback(
            output_id.clone(),
            QueryChangeCodec::schema().descriptor().clone(),
            move |input| async move {
                let context = ExecutionContext::current();
                let trace = context
                    .trace()
                    .context("handled callback has no operation trace context")?;
                println!(
                    "RESULT source={source} query={source}-query trace_id={} span_id={} changes={}",
                    opentelemetry::trace::TraceId::from_bytes(trace.trace_id()),
                    opentelemetry::trace::SpanId::from_bytes(trace.span_id()),
                    input.envelope.changes().operations().len()
                );
                Ok(())
            },
        )?;
        batch = batch
            .query(Box::new(query))
            .sink(Box::new(reaction))
            .bind_stream(endpoint(source, "out")?, stream(source)?)
            .bind_stream(endpoint(&query_id, "out")?, stream(&query_id)?)
            .connect(
                EdgeDefinition::new(endpoint(source, "out")?, endpoint(&query_id, "in")?),
                Box::new(BoundedPipeConfig { capacity: 32 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(&query_id, "out")?, endpoint(&output_id, "in")?),
                Box::new(BoundedPipeConfig { capacity: 32 }),
            );
    }
    let core = DrasiLib::builder()
        .with_id(INSTANCE)
        .with_component_logging(logging)
        .with_telemetry(
            TelemetryOptions::new(settings)
                .with_context_adapter(Arc::new(OpenTelemetryContextAdapter)),
        )
        .with_components(batch.build()?)
        .build()
        .await?;
    let result = async {
        core.start().await?;
        println!("READY HTTP http://127.0.0.1:{http_port}/sources/http/events");
        println!("READY PostgreSQL publication/slot drasi_trace_demo; wait for its slot to be active before inserting");
        println!("TRACES service={SERVICE}; each RESULT prints the trace ID to open in Jaeger");
        let control = core.computation_control()?;
        let mut observed = control.subscribe_observed();
        tokio::select! {
            stopped = tokio::signal::ctrl_c() => stopped.context("wait for Ctrl-C")?,
            failed = observed.wait_for(|graph| graph.components.values().any(|component| component.failure.is_some())) => {
                let graph = failed.context("component observation closed")?;
                let failure = graph.components.values().find_map(|component| component.failure.as_ref())
                    .context("component failure disappeared")?;
                return Err(GraphError::Reported { cause: failure.cause.clone() }.into());
            }
        }
        Ok(())
    }.await;
    let shutdown = core.shutdown().await.map_err(Into::into);
    finish([result, shutdown])
}
