// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Tracing layer for routing logs to component-specific streams.
//!
//! This module provides a custom `tracing_subscriber::Layer` that captures log events
//! and routes them to the appropriate component's log stream based on span context.
//!
//! # How It Works
//!
//! 1. Components create tracing spans with `component_id` and `component_type` attributes
//! 2. Log events (from `tracing::info!()` or `log::info!()` via bridge) occur within these spans
//! 3. `ComponentLogLayer` extracts the component info from the span hierarchy
//! 4. Logs are submitted to the selected runtime's bounded capture worker.
//!
//! Use `ComponentLogRuntime::layer()` for composable, independently filtered
//! capture. Plain library construction initializes the default service separately.
//!
//! # Example
//!
//! ```no_run
//! use tracing::Instrument;
//!
//! # async fn example(source_id: &str) {
//! // Create a span for the component
//! let span = tracing::info_span!(
//!     "source",
//!     component_id = %source_id,
//!     component_type = "source"
//! );
//!
//! // Run code within the span - logs are automatically routed
//! async {
//!     tracing::info!("Starting source");
//!     // or log::info!("Starting source"); - works via tracing-log bridge
//! }.instrument(span).await;
//! # }
//! ```

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_log::NormalizeEvent;
use tracing_subscriber::layer::{Context, Filter};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer};

use super::{ComponentLogRuntime, ComponentLogType, LogLevel, LogMessage, PLUGIN_LOG_TARGET};

/// Tracing layer that routes log events to component-specific streams.
///
/// This layer intercepts all tracing events and checks if they occur within
/// a span that has `component_id` and `component_type` attributes. If so,
/// the log is routed to that component's log stream in the registry.
pub struct ComponentLogLayer {
    runtime: ComponentLogRuntime,
}

impl ComponentLogLayer {
    /// Create a raw capture layer bound to its owner. Prefer
    /// `ComponentLogRuntime::layer()` to also apply the runtime's log filter.
    pub fn new(runtime: ComponentLogRuntime) -> Self {
        Self { runtime }
    }
}

pub(crate) struct ComponentLogFilter(pub EnvFilter);

impl<S> Filter<S> for ComponentLogFilter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn enabled(&self, metadata: &tracing::Metadata<'_>, context: &Context<'_, S>) -> bool {
        metadata.is_span() || Filter::<S>::enabled(&self.0, metadata, context)
    }

    fn callsite_enabled(
        &self,
        metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        let interest = Filter::<S>::callsite_enabled(&self.0, metadata);
        if metadata.is_span() {
            tracing::subscriber::Interest::always()
        } else {
            interest
        }
    }

    fn event_enabled(&self, event: &Event<'_>, context: &Context<'_, S>) -> bool {
        Filter::<S>::event_enabled(&self.0, event, context)
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        context: Context<'_, S>,
    ) {
        Filter::<S>::on_new_span(&self.0, attrs, id, context);
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        context: Context<'_, S>,
    ) {
        Filter::<S>::on_record(&self.0, id, values, context);
    }

    fn on_enter(&self, id: &tracing::span::Id, context: Context<'_, S>) {
        Filter::<S>::on_enter(&self.0, id, context);
    }

    fn on_exit(&self, id: &tracing::span::Id, context: Context<'_, S>) {
        Filter::<S>::on_exit(&self.0, id, context);
    }

    fn on_close(&self, id: tracing::span::Id, context: Context<'_, S>) {
        Filter::<S>::on_close(&self.0, id, context);
    }
}

impl<S> Layer<S> for ComponentLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        // Extract component info from span attributes and cache in extensions
        let mut visitor = ComponentInfoVisitor::default();
        attrs.record(&mut visitor);

        if let Some(info) = visitor.into_component_info() {
            if let Some(span) = ctx.span(id) {
                let mut extensions = span.extensions_mut();
                if extensions.get_mut::<ComponentInfo>().is_none() {
                    extensions.insert(info);
                }
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let normalized = event.normalized_metadata();
        let metadata = normalized.as_ref().unwrap_or_else(|| event.metadata());
        if metadata.target() == PLUGIN_LOG_TARGET {
            return;
        }
        // Try to find component context from current or parent spans
        let component_info = ctx.event_span(event).and_then(|span| {
            // Walk up the span tree to find component info
            let mut current = Some(span);
            while let Some(span_ref) = current {
                if let Some(info) = extract_component_info(&span_ref) {
                    return Some(info);
                }
                current = span_ref.parent();
            }
            None
        });

        // If we found component context, route the log
        if let Some(info) = component_info {
            let level = convert_level(*event.metadata().level());
            let message = extract_message(event);

            let log_message = LogMessage::with_instance(
                level,
                message,
                info.instance_id,
                info.component_id,
                info.component_type,
            );

            self.runtime.try_log(log_message);
        }
    }
}

/// Component info stored in span extensions.
#[derive(Clone)]
struct ComponentInfo {
    instance_id: String,
    component_id: String,
    component_type: ComponentLogType,
}

/// Extract component info from a span's cached extensions.
fn extract_component_info<S>(
    span: &tracing_subscriber::registry::SpanRef<'_, S>,
) -> Option<ComponentInfo>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    // Component info is cached in span extensions during on_new_span
    let extensions = span.extensions();
    extensions.get::<ComponentInfo>().cloned()
}

/// Visitor for extracting component info from span/event fields.
#[derive(Default)]
struct ComponentInfoVisitor {
    instance_id: Option<String>,
    component_id: Option<String>,
    component_type: Option<String>,
}

impl Visit for ComponentInfoVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "instance_id" => {
                self.instance_id = Some(format!("{value:?}").trim_matches('"').to_string())
            }
            "component_id" => {
                self.component_id = Some(format!("{value:?}").trim_matches('"').to_string())
            }
            "component_type" => {
                self.component_type = Some(format!("{value:?}").trim_matches('"').to_string())
            }
            _ => {}
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "instance_id" => self.instance_id = Some(value.to_string()),
            "component_id" => self.component_id = Some(value.to_string()),
            "component_type" => self.component_type = Some(value.to_string()),
            _ => {}
        }
    }
}

impl ComponentInfoVisitor {
    fn into_component_info(self) -> Option<ComponentInfo> {
        let component_id = self.component_id?;
        let component_type = self
            .component_type
            .as_deref()
            .and_then(parse_component_type)?;
        Some(ComponentInfo {
            instance_id: self.instance_id.unwrap_or_default(),
            component_id,
            component_type,
        })
    }
}

/// Parse a log category without changing the lifecycle-event type contract.
fn parse_component_type(s: &str) -> Option<ComponentLogType> {
    ComponentLogType::parse(s)
}

/// Visitor for extracting the message from an event.
#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
    fields: Vec<String>,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}").trim_matches('"').to_string());
        } else {
            self.fields.push(format!("{}={value:?}", field.name()));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields.push(format!("{}={}", field.name(), value));
        }
    }
}

/// Extract the message from a tracing event.
fn extract_message(event: &Event<'_>) -> String {
    let mut visitor = MessageVisitor::default();
    event.record(&mut visitor);

    if let Some(msg) = visitor.message {
        msg
    } else if !visitor.fields.is_empty() {
        visitor.fields.join(", ")
    } else {
        // Fallback: use the event metadata name
        event.metadata().name().to_string()
    }
}

/// Convert tracing Level to our LogLevel.
fn convert_level(level: tracing::Level) -> LogLevel {
    match level {
        tracing::Level::ERROR => LogLevel::Error,
        tracing::Level::WARN => LogLevel::Warn,
        tracing::Level::INFO => LogLevel::Info,
        tracing::Level::DEBUG => LogLevel::Debug,
        tracing::Level::TRACE => LogLevel::Trace,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_component_type() {
        assert_eq!(
            parse_component_type("source"),
            Some(ComponentLogType::Source)
        );
        assert_eq!(
            parse_component_type("Source"),
            Some(ComponentLogType::Source)
        );
        assert_eq!(
            parse_component_type("SOURCE"),
            Some(ComponentLogType::Source)
        );
        assert_eq!(parse_component_type("query"), Some(ComponentLogType::Query));
        assert_eq!(
            parse_component_type("reaction"),
            Some(ComponentLogType::Reaction)
        );
        assert_eq!(
            parse_component_type("transformer"),
            Some(ComponentLogType::Transformer)
        );
        assert_eq!(parse_component_type("SINK"), Some(ComponentLogType::Sink));
        assert_eq!(
            parse_component_type("Service"),
            Some(ComponentLogType::Service)
        );
        assert_eq!(parse_component_type("unknown"), None);
    }

    #[test]
    fn test_convert_level() {
        assert_eq!(convert_level(tracing::Level::ERROR), LogLevel::Error);
        assert_eq!(convert_level(tracing::Level::WARN), LogLevel::Warn);
        assert_eq!(convert_level(tracing::Level::INFO), LogLevel::Info);
        assert_eq!(convert_level(tracing::Level::DEBUG), LogLevel::Debug);
        assert_eq!(convert_level(tracing::Level::TRACE), LogLevel::Trace);
    }
}
