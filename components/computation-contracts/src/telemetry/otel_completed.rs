// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use opentelemetry::{
    trace::{SpanContext, SpanId, SpanKind, TraceFlags, TraceId, TraceState},
    Context, KeyValue,
};
use opentelemetry_sdk::{
    error::{OTelSdkError, OTelSdkResult},
    trace::{ShouldSample, Span, SpanData, SpanProcessor},
    Resource,
};
use std::{
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, UNIX_EPOCH},
};

#[derive(Debug)]
struct Shared<P> {
    processor: Mutex<P>,
    closed: AtomicBool,
}

/// Compose this processor and its sampler into the host provider, and supply a
/// clone to native collection. The provider alone owns exporter shutdown.
#[derive(Debug)]
pub struct OpenTelemetrySpanProcessor<P, S> {
    shared: Arc<Shared<P>>,
    sampler: S,
}

impl<P, S: Clone> Clone for OpenTelemetrySpanProcessor<P, S> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            sampler: self.sampler.clone(),
        }
    }
}

impl<P: SpanProcessor, S: ShouldSample + Clone + 'static> OpenTelemetrySpanProcessor<P, S> {
    pub fn new(processor: P, sampler: S) -> Self {
        Self {
            shared: Arc::new(Shared {
                processor: Mutex::new(processor),
                closed: AtomicBool::new(false),
            }),
            sampler,
        }
    }
    pub fn sampler(&self) -> TelemetrySampler<S> {
        TelemetrySampler(self.sampler.clone())
    }
}

fn attributes(values: TelemetryAttributes) -> Vec<KeyValue> {
    values
        .into_iter()
        .map(|(key, value)| {
            let value = match value {
                TelemetryAttribute::Bool(value) => value.into(),
                TelemetryAttribute::I64(value) => value.into(),
                TelemetryAttribute::F64(value) => value.into(),
                TelemetryAttribute::String(value) => opentelemetry::Value::String(value.into()),
            };
            KeyValue::new(key, value)
        })
        .collect()
}
fn context(value: &TraceContextV1, remote: bool) -> Result<SpanContext> {
    Ok(SpanContext::new(
        TraceId::from_bytes(value.trace_id()),
        SpanId::from_bytes(value.span_id()),
        TraceFlags::new(value.trace_flags()),
        remote,
        value
            .trace_state()
            .map(TraceState::from_str)
            .transpose()
            .map_err(|error| TelemetryError::Invalid(error.to_string()))?
            .unwrap_or_default(),
    ))
}
fn links(values: Vec<TelemetrySpanLink>) -> Result<Vec<opentelemetry::trace::Link>> {
    values
        .into_iter()
        .map(|link| {
            Ok(opentelemetry::trace::Link::new(
                context(&link.context, true)?,
                attributes(link.attributes),
                0,
            ))
        })
        .collect()
}

impl<P: SpanProcessor, S: ShouldSample + Clone + 'static> CompletedSpanProcessor
    for OpenTelemetrySpanProcessor<P, S>
{
    fn sample_root(
        &self,
        config: &PluginTelemetryConfig,
        request: &RootSpanRequest,
    ) -> Result<RootSpanDecision> {
        request.validate()?;
        let mut values = attributes(request.attributes.clone());
        values.extend(
            config
                .scope
                .labels()
                .into_iter()
                .map(|(key, value)| KeyValue::new(key, value)),
        );
        values.push(KeyValue::new("plugin_kind", config.plugin_kind.clone()));
        let result = self.sampler.should_sample(
            None,
            TraceId::from_bytes(request.trace_id),
            &request.name,
            &SpanKind::Internal,
            &values,
            &links(request.links.clone())?,
        );
        let state = result.trace_state.header();
        Ok(RootSpanDecision {
            sampled: result.decision == opentelemetry_sdk::trace::SamplingDecision::RecordAndSample,
            trace_state: (!state.is_empty()).then_some(state),
        })
    }

    fn process(&self, record: CollectedSpan) -> Result<()> {
        record.span.validate()?;
        let span = record.span;
        let mut values = attributes(span.attributes);
        values.extend(
            record
                .scope
                .labels()
                .into_iter()
                .map(|(key, value)| KeyValue::new(key, value)),
        );
        values.push(KeyValue::new("plugin_kind", record.plugin_kind));
        values.push(KeyValue::new(
            "generation",
            i64::try_from(record.generation)
                .map(opentelemetry::Value::I64)
                .unwrap_or_else(|_| {
                    opentelemetry::Value::String(record.generation.to_string().into())
                }),
        ));
        values.push(KeyValue::new("target", span.target.clone()));
        if let Some(role) = record.scope.role() {
            values.push(KeyValue::new("role", role.as_str()));
        }
        let mut events = opentelemetry_sdk::trace::SpanEvents::default();
        events.events = span
            .events
            .into_iter()
            .map(|event| {
                opentelemetry::trace::Event::new(
                    event.name,
                    UNIX_EPOCH + Duration::from_nanos(event.time_unix_ns),
                    attributes(event.attributes),
                    0,
                )
            })
            .collect();
        let mut span_links = opentelemetry_sdk::trace::SpanLinks::default();
        span_links.links = links(span.links)?;
        let data = SpanData {
            span_context: context(&span.context, false)?,
            parent_span_id: span
                .parent_span_id
                .map(SpanId::from_bytes)
                .unwrap_or(SpanId::INVALID),
            parent_span_is_remote: span.parent_span_id.is_some(),
            span_kind: SpanKind::Internal,
            name: span.name.into(),
            start_time: UNIX_EPOCH + Duration::from_nanos(span.start_time_unix_ns),
            end_time: UNIX_EPOCH + Duration::from_nanos(span.end_time_unix_ns),
            attributes: values,
            dropped_attributes_count: 0,
            events,
            links: span_links,
            status: match span.status {
                TelemetrySpanStatus::Unset => opentelemetry::trace::Status::Unset,
                TelemetrySpanStatus::Ok => opentelemetry::trace::Status::Ok,
                TelemetrySpanStatus::Error => opentelemetry::trace::Status::error(""),
            },
            instrumentation_scope: opentelemetry::InstrumentationScope::builder(span.target)
                .build(),
        };
        let processor = self
            .shared
            .processor
            .lock()
            .map_err(|_| TelemetryError::Poisoned)?;
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(TelemetryError::Retired);
        }
        processor.on_end(data);
        Ok(())
    }
}

impl<P: SpanProcessor, S: ShouldSample + Clone + 'static> SpanProcessor
    for OpenTelemetrySpanProcessor<P, S>
{
    fn on_start(&self, span: &mut Span, context: &Context) {
        match self.shared.processor.lock() {
            Ok(processor) => processor.on_start(span, context),
            Err(error) => log::error!("Host span processor poisoned: {error}"),
        }
    }
    fn on_end(&self, span: SpanData) {
        match self.shared.processor.lock() {
            Ok(processor) => {
                if self.shared.closed.load(Ordering::Acquire) {
                    log::error!("Host span submitted after processor shutdown");
                } else {
                    processor.on_end(span);
                }
            }
            Err(error) => log::error!("Host span processor poisoned: {error}"),
        }
    }
    fn force_flush(&self) -> OTelSdkResult {
        self.shared
            .processor
            .lock()
            .map_err(|error| OTelSdkError::InternalFailure(error.to_string()))?
            .force_flush()
    }
    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        let processor = self
            .shared
            .processor
            .lock()
            .map_err(|error| OTelSdkError::InternalFailure(error.to_string()))?;
        if !self.shared.closed.load(Ordering::Acquire) {
            processor.shutdown_with_timeout(timeout)?;
            self.shared.closed.store(true, Ordering::Release);
        }
        Ok(())
    }
    fn set_resource(&mut self, resource: &Resource) {
        match self.shared.processor.lock() {
            Ok(mut processor) => processor.set_resource(resource),
            Err(error) => log::error!("Host span processor resource binding failed: {error}"),
        }
    }
}
