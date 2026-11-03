// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    ExecutionContext, Result, TelemetryError, TraceContextAdapter, TraceContextGuard,
    TraceContextV1,
};
use opentelemetry::{
    trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState},
    Context,
};
use std::str::FromStr;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Adapts a host-composed `tracing-opentelemetry` layer with its default context
/// activation enabled. Installs nothing.
#[derive(Debug, Default)]
pub struct OpenTelemetryContextAdapter;

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
