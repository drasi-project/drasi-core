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
//! 4. Logs are routed synchronously to `ComponentLogRegistry` for storage and broadcast
//!
//! # Global Registry
//!
//! Since `tracing` uses a single global subscriber per process, we use a shared global
//! `ComponentLogRegistry` that all `DrasiLib` instances can access. Call `get_or_init_global_registry()`
//! to get the shared registry, which will be initialized on first use.
//!
//! # Example
//!
//! ```ignore
//! use tracing::Instrument;
//!
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
//! ```

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use super::component_log::{ComponentLogRegistry, LogLevel, LogMessage};
use crate::channels::ComponentType;

use std::sync::OnceLock;

/// Default capacity for the log message channel.
/// This provides backpressure when logging volume is high.
const LOG_CHANNEL_CAPACITY: usize = 10_000;

/// Global log registry shared by all DrasiLib instances.
/// Since tracing uses a single global subscriber, we need a single shared registry.
static GLOBAL_LOG_REGISTRY: OnceLock<Arc<ComponentLogRegistry>> = OnceLock::new();

/// Global sender for the log worker. Initialized alongside the registry.
static GLOBAL_LOG_SENDER: OnceLock<mpsc::Sender<LogMessage>> = OnceLock::new();

/// Get or initialize the shared global log registry.
///
/// This returns a shared registry that all DrasiLib instances use. The tracing
/// subscriber is global (per-process), so all logs from all DrasiLib instances
/// go to the same registry.
///
/// On first call, this initializes the tracing subscriber with the registry
/// and spawns a background worker to process log messages.
pub fn get_or_init_global_registry() -> Arc<ComponentLogRegistry> {
    GLOBAL_LOG_REGISTRY
        .get_or_init(|| {
            let registry = Arc::new(ComponentLogRegistry::new());

            // Create bounded channel for log messages
            let (tx, rx) = mpsc::channel::<LogMessage>(LOG_CHANNEL_CAPACITY);

            // Store sender globally for the tracing layer to use
            let _ = GLOBAL_LOG_SENDER.set(tx);

            // Spawn the log worker in a dedicated thread with its own runtime.
            // This ensures the worker is independent of any caller's runtime.
            spawn_log_worker(registry.clone(), rx);

            // Initialize tracing subscriber
            init_tracing_internal(registry.clone());

            registry
        })
        .clone()
}

/// Spawn the background worker that processes log messages.
///
/// This worker drains the channel and writes logs to the registry.
/// Uses a dedicated thread with its own tokio runtime to ensure
/// independence from the caller's async context.
fn spawn_log_worker(registry: Arc<ComponentLogRegistry>, mut rx: mpsc::Receiver<LogMessage>) {
    std::thread::Builder::new()
        .name("drasi-log-worker".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create log worker runtime");

            rt.block_on(async move {
                while let Some(message) = rx.recv().await {
                    registry.log(message).await;
                }
            });
        })
        .expect("Failed to spawn log worker thread");
}

/// Initialize the tracing subscriber with component log routing.
///
/// This sets up:
/// - `ComponentLogLayer` for routing logs to component-specific streams
/// - `fmt::layer()` for console output
/// - `EnvFilter` for level control via `RUST_LOG` environment variable
/// - `tracing-log` bridge for `log` crate compatibility
///
/// # Arguments
///
/// * `log_registry` - The registry to route component logs to
///
/// # Example
///
/// ```ignore
/// use drasi_lib::managers::ComponentLogRegistry;
/// use std::sync::Arc;
///
/// let log_registry = Arc::new(ComponentLogRegistry::new());
/// drasi_lib::init_tracing(log_registry.clone());
///
/// // Now both tracing::info!() and log::info!() work
/// tracing::info!("Hello from tracing");
/// log::info!("Hello from log crate");
/// ```
///
/// # Note
///
/// If another `log` crate logger was initialized before calling this function,
/// `log::info!()` calls will go to that logger instead. However, `tracing::info!()`
/// calls will still be captured correctly.
///
/// # Deprecated
///
/// Prefer using `get_or_init_global_registry()` which handles initialization automatically.
/// This function is kept for backward compatibility.
pub fn init_tracing(log_registry: Arc<ComponentLogRegistry>) {
    // Ensure global registry is initialized (which sets up the channel worker)
    let _ = get_or_init_global_registry();

    // If caller provided a different registry, warn them
    // (can't actually use it since tracing subscriber is already set)
    if !Arc::ptr_eq(&log_registry, &get_or_init_global_registry()) {
        tracing::warn!(
            "init_tracing called with custom registry, but global registry already initialized. \
             The provided registry will be ignored. Use get_or_init_global_registry() instead."
        );
    }
}

/// Internal initialization - sets up tracing subscriber without channel/worker.
fn init_tracing_internal(log_registry: Arc<ComponentLogRegistry>) {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter};

    // Try to install the log->tracing bridge
    // Use init() which returns Result, ignore error if logger already set
    let _ = tracing_log::LogTracer::init();

    // Build the subscriber with our custom layer
    // Use RUST_LOG if set, otherwise default to INFO level
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(ComponentLogLayer::new(log_registry))
        .with(fmt::layer().with_target(true).with_level(true));

    // Try to set as the global subscriber
    // Use try_init to handle case where subscriber is already set
    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// Try to initialize tracing, returning whether initialization succeeded.
///
/// Unlike `init_tracing()`, this returns `false` if a subscriber is already set,
/// allowing callers to handle this case.
///
/// # Deprecated
///
/// Prefer using `get_or_init_global_registry()` which handles initialization automatically.
pub fn try_init_tracing(log_registry: Arc<ComponentLogRegistry>) -> bool {
    // Check if already initialized
    if GLOBAL_LOG_REGISTRY.get().is_some() {
        return false;
    }

    // Initialize via the standard path
    let _ = get_or_init_global_registry();

    // Warn if caller's registry differs
    if !Arc::ptr_eq(&log_registry, &get_or_init_global_registry()) {
        tracing::warn!(
            "try_init_tracing called with custom registry, but initialization uses global registry. \
             The provided registry will be ignored."
        );
    }

    true
}

/// Tracing layer that routes log events to component-specific streams.
///
/// This layer intercepts all tracing events and checks if they occur within
/// a span that has `component_id` and `component_type` attributes. If so,
/// the log is routed to that component's log stream in the registry.
pub struct ComponentLogLayer {
    registry: Arc<ComponentLogRegistry>,
}

impl ComponentLogLayer {
    /// Create a new layer with the given log registry.
    pub fn new(registry: Arc<ComponentLogRegistry>) -> Self {
        Self { registry }
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
                extensions.insert(info);
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
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

            // Send to the log worker via bounded channel
            // This provides backpressure instead of spawning unbounded tasks
            if let Some(sender) = GLOBAL_LOG_SENDER.get() {
                // Use try_send to avoid blocking in the tracing layer
                // If channel is full, log is dropped (better than OOM)
                if sender.try_send(log_message).is_err() {
                    // Channel full or closed - log still goes to console via fmt layer
                    // Could add a metric here for monitoring dropped logs
                }
            }
        }
    }
}

/// Component info stored in span extensions.
#[derive(Clone)]
struct ComponentInfo {
    instance_id: String,
    component_id: String,
    component_type: ComponentType,
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

/// Parse a component type string into a ComponentType enum.
fn parse_component_type(s: &str) -> Option<ComponentType> {
    match s.to_lowercase().as_str() {
        "source" => Some(ComponentType::Source),
        "query" => Some(ComponentType::Query),
        "reaction" => Some(ComponentType::Reaction),
        _ => None,
    }
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
        assert_eq!(parse_component_type("source"), Some(ComponentType::Source));
        assert_eq!(parse_component_type("Source"), Some(ComponentType::Source));
        assert_eq!(parse_component_type("SOURCE"), Some(ComponentType::Source));
        assert_eq!(parse_component_type("query"), Some(ComponentType::Query));
        assert_eq!(
            parse_component_type("reaction"),
            Some(ComponentType::Reaction)
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
