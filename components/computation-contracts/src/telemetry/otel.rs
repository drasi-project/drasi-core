// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    ExecutionContext, Result, RootSampling, TelemetryError, TraceContextAdapter, TraceContextGuard,
    TraceContextV1,
};
use opentelemetry::{
    trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState},
    Context,
};
use std::{
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Adapts a host-composed `tracing-opentelemetry` layer with its default context
/// activation enabled. Installs nothing.
#[derive(Debug, Default)]
pub struct OpenTelemetryContextAdapter;

#[derive(Debug)]
struct OperationSampling {
    policy: RootSampling,
    observed: AtomicBool,
}

/// Compose around the host's root sampler to support per-graph root policies.
/// Parent decisions are always inherited; bounded links are available to `inner`.
#[derive(Debug, Clone)]
pub struct TelemetrySampler<S>(pub S);

impl<S: opentelemetry_sdk::trace::ShouldSample + Clone + 'static>
    opentelemetry_sdk::trace::ShouldSample for TelemetrySampler<S>
{
    fn should_sample(
        &self,
        parent: Option<&Context>,
        trace_id: TraceId,
        name: &str,
        kind: &opentelemetry::trace::SpanKind,
        attributes: &[opentelemetry::KeyValue],
        links: &[opentelemetry::trace::Link],
    ) -> opentelemetry_sdk::trace::SamplingResult {
        use opentelemetry_sdk::trace::{Sampler, SamplingDecision, SamplingResult};
        let request = parent.and_then(|context| context.get::<Arc<OperationSampling>>());
        if let Some(request) = request {
            request.observed.store(true, Ordering::Relaxed);
        }
        if let Some(parent) = parent {
            let span = parent.span();
            let context = span.span_context();
            if context.is_valid() {
                return SamplingResult {
                    decision: if context.is_sampled() {
                        SamplingDecision::RecordAndSample
                    } else {
                        SamplingDecision::Drop
                    },
                    attributes: vec![],
                    trace_state: context.trace_state().clone(),
                };
            }
        }
        match request
            .map(|request| request.policy)
            .unwrap_or(RootSampling::Always)
        {
            RootSampling::Always => self
                .0
                .should_sample(parent, trace_id, name, kind, attributes, links),
            RootSampling::Never => {
                Sampler::AlwaysOff.should_sample(parent, trace_id, name, kind, attributes, links)
            }
            RootSampling::Ratio(ratio) => Sampler::TraceIdRatioBased(ratio.value())
                .should_sample(parent, trace_id, name, kind, attributes, links),
        }
    }
}

impl TraceContextGuard for opentelemetry::ContextGuard {}

fn span_context(trace: &TraceContextV1) -> SpanContext {
    SpanContext::new(
        TraceId::from_bytes(trace.trace_id()),
        SpanId::from_bytes(trace.span_id()),
        TraceFlags::new(trace.trace_flags()),
        true,
        trace
            .trace_state()
            .map_or_else(TraceState::default, |state| {
                TraceState::from_str(state).expect("validated portable W3C trace state")
            }),
    )
}

fn context(value: &ExecutionContext) -> Context {
    value.trace().map_or_else(Context::new, |trace| {
        Context::new().with_remote_span_context(span_context(trace))
    })
}

fn portable(context: &Context) -> ExecutionContext {
    let span = context.span();
    let span = span.span_context();
    if !span.is_valid() {
        return ExecutionContext::default();
    }
    let state = span.trace_state().header();
    match TraceContextV1::new(
        span.trace_id().to_bytes(),
        span.span_id().to_bytes(),
        span.trace_flags().to_u8(),
        (!state.is_empty()).then(|| state.into()),
    ) {
        Ok(trace) => ExecutionContext::new(trace),
        Err(error) => {
            log::warn!("Rejecting invalid OpenTelemetry execution context: {error}");
            ExecutionContext::default()
        }
    }
}

impl TraceContextAdapter for OpenTelemetryContextAdapter {
    fn enter(&self, value: &ExecutionContext) -> Box<dyn TraceContextGuard> {
        Box::new(context(value).attach())
    }

    fn current_context(&self) -> ExecutionContext {
        portable(&Context::current())
    }

    fn span_context(&self, span: &tracing::Span) -> ExecutionContext {
        portable(&span.context())
    }

    fn set_parent(&self, span: &tracing::Span, parent: &ExecutionContext) -> Result<()> {
        span.set_parent(context(parent))
            .map_err(|error| TelemetryError::Invalid(format!("cannot set trace parent: {error}")))
    }

    fn add_link(&self, span: &tracing::Span, trace: &TraceContextV1) -> Result<()> {
        span.add_link(span_context(trace));
        Ok(())
    }

    fn prepare_operation(
        &self,
        span: &tracing::Span,
        parent: &ExecutionContext,
        links: &[TraceContextV1],
        sampling: RootSampling,
    ) -> Result<()> {
        let request = Arc::new(OperationSampling {
            policy: sampling,
            observed: AtomicBool::new(false),
        });
        span.set_parent(context(parent).with_value(request.clone()))
            .map_err(|error| {
                TelemetryError::Invalid(format!("cannot prepare operation parent: {error}"))
            })?;
        for link in links {
            self.add_link(span, link)?;
        }
        let _context = span.context();
        if let (Some(parent), Some(child)) = (parent.trace(), portable(&_context).trace()) {
            if parent.sampled() != child.sampled() {
                return Err(TelemetryError::Unsupported(
                    "parent-based host operation sampler",
                ));
            }
        }
        if parent.is_empty()
            && sampling != RootSampling::Always
            && _context.span().span_context().is_valid()
            && !request.observed.load(Ordering::Relaxed)
        {
            return Err(TelemetryError::Unsupported(
                "host TelemetrySampler for configured root sampling",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::ExecutionScope;
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SdkTracerProvider};
    use std::sync::Arc;
    use tracing::Instrument;
    use tracing_subscriber::prelude::*;

    #[derive(Debug, Clone, Default)]
    struct RecordingSampler(Arc<std::sync::Mutex<Vec<(String, usize, bool)>>>);

    impl opentelemetry_sdk::trace::ShouldSample for RecordingSampler {
        fn should_sample(
            &self,
            parent: Option<&Context>,
            trace_id: TraceId,
            name: &str,
            kind: &opentelemetry::trace::SpanKind,
            attributes: &[opentelemetry::KeyValue],
            links: &[opentelemetry::trace::Link],
        ) -> opentelemetry_sdk::trace::SamplingResult {
            self.0.lock().unwrap().push((
                name.to_owned(),
                links.len(),
                attributes.iter().any(|attribute| {
                    attribute.key.as_str() == "links_truncated"
                        && attribute.value == opentelemetry::Value::Bool(true)
                }),
            ));
            opentelemetry_sdk::trace::ShouldSample::should_sample(
                &Sampler::AlwaysOn,
                parent,
                trace_id,
                name,
                kind,
                attributes,
                links,
            )
        }
    }

    #[test]
    fn operation_links_and_attributes_precede_one_root_decision_and_unsampled_context_propagates() {
        use crate::telemetry::{BatchTraceParent, TraceLinkLimits};
        let exporter = InMemorySpanExporter::default();
        let sampler = RecordingSampler::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .with_sampler(TelemetrySampler(sampler.clone()))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("operations")));
        tracing::subscriber::with_default(subscriber, || {
            let one = ExecutionContext::new(TraceContextV1::new([1; 16], [1; 8], 1, None).unwrap());
            let two = ExecutionContext::new(TraceContextV1::new([2; 16], [2; 8], 0, None).unwrap());
            let selected = BatchTraceParent::select(
                [&one, &two],
                TraceLinkLimits {
                    max_links: 1,
                    max_bytes: 4096,
                },
            );
            let span = tracing::info_span!(parent: None, "sink.handle_batch", links_truncated = selected.links_truncated);
            OpenTelemetryContextAdapter
                .prepare_operation(
                    &span,
                    &selected.parent,
                    &selected.links,
                    RootSampling::Always,
                )
                .unwrap();
            let batch = OpenTelemetryContextAdapter.span_context(&span);
            assert!(batch.trace().unwrap().sampled());
            assert_ne!(batch.trace().unwrap().trace_id(), [1; 16]);
            assert_eq!(
                sampler.0.lock().unwrap().as_slice(),
                &[("sink.handle_batch".into(), 1, true)]
            );
            let child = tracing::info_span!(parent: None, "child");
            OpenTelemetryContextAdapter
                .prepare_operation(&child, &batch, &[], RootSampling::Never)
                .unwrap();
            assert!(OpenTelemetryContextAdapter
                .span_context(&child)
                .trace()
                .unwrap()
                .sampled());
            assert_eq!(
                sampler.0.lock().unwrap().len(),
                1,
                "child must not consult the root sampler"
            );
            for policy in [
                RootSampling::Never,
                RootSampling::Ratio(crate::telemetry::SamplingRatio::new(0.0).unwrap()),
            ] {
                let root = tracing::info_span!(parent: None, "unsampled");
                OpenTelemetryContextAdapter
                    .prepare_operation(&root, &ExecutionContext::default(), &selected.links, policy)
                    .unwrap();
                let context = OpenTelemetryContextAdapter.span_context(&root);
                assert!(!context.is_empty());
                assert!(!context.trace().unwrap().sampled());
                let child = tracing::info_span!(parent: None, "unsampled.child");
                OpenTelemetryContextAdapter
                    .prepare_operation(&child, &context, &[], RootSampling::Always)
                    .unwrap();
                assert!(!OpenTelemetryContextAdapter
                    .span_context(&child)
                    .trace()
                    .unwrap()
                    .sampled());
            }
        });
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(
            spans
                .iter()
                .find(|span| span.name == "sink.handle_batch")
                .unwrap()
                .links
                .links
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn real_otel_parentage_unsampled_and_explicit_empty_scope() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .with_sampler(Sampler::ParentBased(Box::new(Sampler::AlwaysOn)))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("context-test")));
        let dispatch = tracing::Dispatch::new(subscriber);
        let adapter = Arc::new(OpenTelemetryContextAdapter);
        let _subscriber = tracing::dispatcher::set_default(&dispatch);
        let unrelated = tracing::info_span!("unrelated");
        let unrelated_context = adapter.span_context(&unrelated);
        async {
            for flags in [0, 1] {
                let parent = ExecutionContext::new(
                    TraceContextV1::new([7; 16], [8; 8], flags, None).unwrap(),
                );
                let scope = ExecutionScope::new(parent.clone()).with_adapter(Some(adapter.clone()));
                scope
                    .scope(async {
                        assert_eq!(ExecutionContext::current(), parent);
                        let child = tracing::info_span!("receiver");
                        let child_context = adapter.span_context(&child);
                        let trace = child_context.trace().unwrap();
                        assert_eq!(trace.trace_id(), [7; 16]);
                        assert_ne!(trace.span_id(), [8; 8]);
                        assert_eq!(trace.sampled(), flags == 1);
                        async {
                            tokio::task::yield_now().await;
                            assert_eq!(ExecutionContext::current(), child_context);
                            let nested = tracing::info_span!("nested");
                            let nested_context = adapter.span_context(&nested);
                            assert_eq!(nested_context.trace().unwrap().trace_id(), [7; 16]);
                            drop(nested);
                        }
                        .instrument(child)
                        .await;
                        assert_eq!(ExecutionContext::current(), parent);
                    })
                    .await;
            }
            ExecutionScope::new(ExecutionContext::default())
                .with_adapter(Some(adapter.clone()))
                .scope(async {
                    assert!(ExecutionContext::current().is_empty());
                    let root = tracing::info_span!("explicit-empty");
                    let root_context = adapter.span_context(&root);
                    assert_ne!(
                        root_context.trace().unwrap().trace_id(),
                        unrelated_context.trace().unwrap().trace_id()
                    );
                })
                .await;
            assert_eq!(adapter.current_context(), unrelated_context);
        }
        .instrument(unrelated)
        .await;
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let receivers = spans
            .iter()
            .filter(|span| span.name == "receiver")
            .collect::<Vec<_>>();
        assert_eq!(
            receivers.len(),
            1,
            "unsampled receiver must not be exported"
        );
        assert_eq!(receivers[0].parent_span_id.to_bytes(), [8; 8]);
        let nested = spans.iter().find(|span| span.name == "nested").unwrap();
        assert_eq!(nested.parent_span_id, receivers[0].span_context.span_id());
        let empty = spans
            .iter()
            .find(|span| span.name == "explicit-empty")
            .unwrap();
        assert_eq!(empty.parent_span_id, SpanId::INVALID);
        let source = tracing::info_span!("closed-source");
        let queued_context = adapter.span_context(&source);
        drop(source);
        assert!(exporter
            .get_finished_spans()
            .unwrap()
            .iter()
            .any(|span| span.name == "closed-source"));
        assert!(
            queued_context.trace().is_some(),
            "portable context outlives the sender without retaining its span"
        );
    }
}
