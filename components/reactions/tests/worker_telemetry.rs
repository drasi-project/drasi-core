// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use drasi_lib::{
    computation::v1::{
        ComponentDescription, ComponentDescriptor, ComponentSemanticKind, ComputationComponent,
        ComputationGraph, ComputationService, DisposeContext, GraphError, StartContext,
        StopContext,
    },
    observability::{
        ExecutionContext, ExecutionScope, MetricRegistrationContext, TelemetryOptions,
        TelemetryProfile, TelemetrySettings, TraceContextAdapter, TraceContextGuard,
        TraceContextV1,
    },
    ComponentId, ComponentName, MemoryStateStoreProvider, Reaction,
};
use tokio::sync::Notify;
use tracing::{
    field::{Field, Visit},
    instrument::WithSubscriber,
    span::{Attributes, Id, Record},
    Subscriber,
};
use tracing_subscriber::{layer::Context, prelude::*, registry::LookupSpan, Layer};

#[path = "../../../lib/tests/computation_support/metrics.rs"]
mod metric_recording;

/// The query UUID used by `lifecycle_support::result`.
pub const QUERY_ID: &str = "00000000-0000-0000-0000-00000000002b";

fn query_id() -> ComponentId {
    ComponentId::try_new(QUERY_ID).expect("fixture query UUID")
}

pub fn context(value: u8) -> ExecutionContext {
    ExecutionContext::new(TraceContextV1::new([value; 16], [value; 8], 1, None).unwrap())
}

pub async fn enqueue(
    reaction: &dyn Reaction,
    result: drasi_lib::channels::QueryResult,
    context: ExecutionContext,
) {
    ExecutionScope::new(context)
        .scope(reaction.enqueue_query_result(result))
        .await
        .unwrap();
}

#[derive(Clone, Debug)]
pub struct Parent {
    id: u64,
    pub context: ExecutionContext,
    pub links: Vec<TraceContextV1>,
}

#[derive(Default, Debug)]
struct Parents(Mutex<Vec<Parent>>);

struct Guard;
impl TraceContextGuard for Guard {}

impl TraceContextAdapter for Parents {
    fn enter(&self, _: &ExecutionContext) -> Box<dyn TraceContextGuard> {
        Box::new(Guard)
    }

    fn current_context(&self) -> ExecutionContext {
        ExecutionContext::default()
    }

    fn span_context(&self, _: &tracing::Span) -> ExecutionContext {
        ExecutionContext::default()
    }

    fn set_parent(
        &self,
        span: &tracing::Span,
        context: &ExecutionContext,
    ) -> Result<(), drasi_lib::observability::TelemetryError> {
        if span
            .metadata()
            .is_some_and(|meta| meta.name().starts_with("reaction.process"))
        {
            self.0.lock().unwrap().push(Parent {
                id: span.id().unwrap().into_u64(),
                context: context.clone(),
                links: Vec::new(),
            });
        }
        Ok(())
    }

    fn add_link(
        &self,
        span: &tracing::Span,
        context: &TraceContextV1,
    ) -> Result<(), drasi_lib::observability::TelemetryError> {
        if let Some(parent) = self
            .0
            .lock()
            .unwrap()
            .iter_mut()
            .rev()
            .find(|parent| Some(parent.id) == span.id().map(|id| id.into_u64()))
        {
            parent.links.push(context.clone());
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Finished {
    pub name: &'static str,
    pub fields: BTreeMap<String, String>,
}

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
}

#[derive(Clone, Default)]
struct Spans {
    finished: Arc<Mutex<Vec<Finished>>>,
    changed: Arc<Notify>,
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Spans {
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        if attributes.metadata().name().starts_with("reaction.process") {
            let mut fields = Fields::default();
            attributes.record(&mut fields);
            context.span(id).unwrap().extensions_mut().insert(fields);
        }
    }

    fn on_record(&self, id: &Id, record: &Record<'_>, context: Context<'_, S>) {
        if let Some(fields) = context
            .span(id)
            .unwrap()
            .extensions_mut()
            .get_mut::<Fields>()
        {
            record.record(fields);
        }
    }

    fn on_close(&self, id: Id, context: Context<'_, S>) {
        let span = context.span(&id).unwrap();
        if let Some(fields) = span.extensions_mut().remove::<Fields>() {
            self.finished.lock().unwrap().push(Finished {
                name: span.name(),
                fields: fields.0,
            });
            self.changed.notify_waiters();
        };
    }
}

struct Worker {
    descriptor: ComponentDescriptor,
    reaction: Arc<dyn Reaction>,
    started: Arc<Notify>,
}

#[async_trait]
impl ComputationComponent for Worker {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn description(&self) -> ComponentDescription {
        ComponentDescription::undeclared()
    }

    async fn start(&mut self, _context: &StartContext) -> anyhow::Result<()> {
        let (updates, _receiver) = tokio::sync::mpsc::channel(64);
        ExecutionScope::capture()
            .with_context(context(99))
            .scope(self.reaction.bind_runtime_context(
                drasi_lib::context::ReactionRuntimeContext::new(
                    "worker-tests",
                    self.reaction.id().clone(),
                    vec![query_id()],
                    Some(Arc::new(MemoryStateStoreProvider::new())),
                    updates,
                    None,
                ),
            ))
            .await;
        self.reaction.initialize().await?;
        self.reaction.start().await?;
        self.started.notify_one();
        Ok(())
    }

    async fn stop(&mut self, _context: &StopContext) -> anyhow::Result<()> {
        self.reaction.stop().await
    }

    drasi_lib::passive_component_lifecycle!(@hook initialize, InitializeContext);
    drasi_lib::passive_component_lifecycle!(@hook pause, PauseContext);
    drasi_lib::passive_component_lifecycle!(@hook resume, ResumeContext);

    async fn dispose(&mut self, context: &DisposeContext) -> anyhow::Result<()> {
        if context.erases_owned_state() {
            anyhow::bail!("this component does not support permanent state removal");
        }
        Ok(())
    }
}

#[async_trait]
impl ComputationService for Worker {
    async fn run(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

#[derive(Default)]
pub struct Recording {
    spans: Spans,
    parents: Arc<Parents>,
    pub metrics: metric_recording::RecordingMetrics,
}

impl Recording {
    pub fn finished(&self) -> Vec<Finished> {
        self.spans.finished.lock().unwrap().clone()
    }

    pub fn parents(&self) -> Vec<Parent> {
        self.parents.0.lock().unwrap().clone()
    }

    pub async fn wait_for_finished(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let changed = self.spans.changed.notified();
                if self.finished().len() >= count {
                    return;
                }
                changed.await;
            }
        })
        .await
        .unwrap();
    }

    pub async fn run(&self, reaction: Arc<dyn Reaction>, work: impl Future<Output = ()>) {
        self.run_with_metrics(reaction, work, false).await;
    }

    pub async fn run_metrics(&self, reaction: Arc<dyn Reaction>, work: impl Future<Output = ()>) {
        self.run_with_metrics(reaction, work, true).await;
    }

    async fn run_with_metrics(
        &self,
        reaction: Arc<dyn Reaction>,
        work: impl Future<Output = ()>,
        metrics: bool,
    ) {
        async {
            let started = Arc::new(Notify::new());
            let mut graph = ComputationGraph::builder("worker-tests")
                .with_telemetry(
                    TelemetryOptions::new(
                        TelemetrySettings::new(if metrics {
                            TelemetryProfile::Persistence
                        } else {
                            TelemetryProfile::Debug
                        })
                        .with_metrics(metrics)
                        .with_tracing(true),
                    )
                    .with_instance_id("worker-tests")
                    .with_metric_context(
                        MetricRegistrationContext::new(
                            Arc::new(self.metrics.clone()),
                            Default::default(),
                        )
                        .unwrap(),
                    )
                    .with_context_adapter(self.parents.clone()),
                )
                .service(
                    ComponentName::try_new("worker").unwrap(),
                    Box::new(Worker {
                        descriptor: ComponentDescriptor::try_new(reaction.id().clone(), Vec::new())
                            .unwrap()
                            .with_semantic_kind(ComponentSemanticKind::Reaction),
                        reaction,
                        started: started.clone(),
                    }),
                )
                .build()
                .unwrap();
            let run = graph.start().unwrap();
            let control = run.control();
            let (result, ()) = tokio::join!(run, async {
                tokio::time::timeout(Duration::from_secs(5), started.notified())
                    .await
                    .unwrap();
                work.await;
                control.cancel();
            });
            assert!(matches!(result, Err(GraphError::Cancelled)));
            graph.shutdown().await.unwrap();
        }
        .with_subscriber(tracing_subscriber::registry().with(self.spans.clone()))
        .await;
    }
}
