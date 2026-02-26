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

//! Tracing subscriber layer that bridges `tracing` events from cdylib plugins
//! to the host via FFI log callbacks.
//!
//! When a plugin spawns background tasks (e.g., postgres replication), those tasks
//! often use `.instrument(span)` to carry context. The `log` crate's FfiLogger
//! cannot see tracing span context, so logs from background tasks lose their
//! instance_id/component_id routing information.
//!
//! This layer solves that by:
//! 1. Registering as a tracing subscriber inside the cdylib plugin
//! 2. Extracting `instance_id`, `component_id`, `component_type` from parent spans
//! 3. Forwarding log events through the same FFI log callback with correct routing IDs

use std::ffi::c_void;
use std::fmt;
use std::sync::atomic::{AtomicPtr, Ordering};

use tracing::field::{Field, Visit};
use tracing::Subscriber;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use super::callbacks::{FfiLogEntry, FfiLogLevel, LogCallbackFn};
use super::types::FfiStr;
use super::vtable_gen::{current_instance_log_ctx, InstanceLogContext};

/// A tracing Layer that forwards events through FFI log callbacks.
///
/// It extracts `instance_id`, `component_id`, and `component_type` from the
/// current span hierarchy (set by `.instrument(span)`) and includes them in
/// the FfiLogEntry so the host can route logs to the correct ComponentLogRegistry key.
pub struct FfiTracingLayer {
    /// Global log callback pointer (shared with FfiLogger via AtomicPtr)
    log_cb: &'static AtomicPtr<()>,
    /// Global log context pointer
    log_ctx: &'static AtomicPtr<c_void>,
    /// Plugin ID string (e.g., "postgres")
    plugin_id: &'static str,
}

impl FfiTracingLayer {
    /// Create a new FfiTracingLayer that reads callbacks from the given AtomicPtrs.
    pub fn new(
        log_cb: &'static AtomicPtr<()>,
        log_ctx: &'static AtomicPtr<c_void>,
        plugin_id: &'static str,
    ) -> Self {
        Self {
            log_cb,
            log_ctx,
            plugin_id,
        }
    }
}

impl<S> Layer<S> for FfiTracingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        // Cache component info in span extensions for fast lookup later
        let mut visitor = ComponentFieldVisitor::default();
        attrs.record(&mut visitor);

        if visitor.has_component_info() {
            if let Some(span) = ctx.span(id) {
                let info = SpanComponentInfo {
                    instance_id: visitor.instance_id.unwrap_or_default(),
                    component_id: visitor.component_id.unwrap_or_default(),
                    component_type: visitor.component_type.unwrap_or_default(),
                };
                span.extensions_mut().insert(info);
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        // Extract the message from the event
        let mut message_visitor = MessageVisitor::default();
        event.record(&mut message_visitor);
        let message = message_visitor.message;

        // Convert tracing level to FfiLogLevel
        let level = match *event.metadata().level() {
            tracing::Level::ERROR => FfiLogLevel::Error,
            tracing::Level::WARN => FfiLogLevel::Warn,
            tracing::Level::INFO => FfiLogLevel::Info,
            tracing::Level::DEBUG => FfiLogLevel::Debug,
            tracing::Level::TRACE => FfiLogLevel::Trace,
        };

        // Extract component info from span hierarchy (set by .instrument(span))
        let span_info = ctx.event_span(event).and_then(|span| {
            if let Some(info) = span.extensions().get::<SpanComponentInfo>() {
                return Some(info.clone());
            }
            for ancestor in span.scope().skip(1) {
                if let Some(info) = ancestor.extensions().get::<SpanComponentInfo>() {
                    return Some(info.clone());
                }
            }
            None
        });

        // Determine instance_id and component_id from (in priority order):
        // 1. TLS InstanceLogContext (set during start/stop vtable calls)
        // 2. Tracing span hierarchy (set by .instrument(span) in background tasks)
        // 3. Empty (global fallback — logs still reach host but without routing)
        let (instance_id, component_id) =
            if let Some(inst_ctx) = current_instance_log_ctx() {
                (inst_ctx.instance_id.clone(), inst_ctx.component_id.clone())
            } else if let Some(ref info) = span_info {
                (info.instance_id.clone(), info.component_id.clone())
            } else {
                (String::new(), String::new())
            };

        // Use per-instance callback if TLS context has one
        if let Some(inst_ctx) = current_instance_log_ctx() {
            if let Some(cb) = inst_ctx.log_cb {
                let entry = FfiLogEntry {
                    level,
                    plugin_id: FfiStr::from_str(self.plugin_id),
                    target: FfiStr::from_str(event.metadata().target()),
                    message: FfiStr::from_str(&message),
                    timestamp_us: super::types::now_us(),
                    instance_id: FfiStr::from_str(&instance_id),
                    component_id: FfiStr::from_str(&component_id),
                };
                cb(inst_ctx.log_ctx, &entry);
                return;
            }
        }

        // Fall back to global callback
        let cb_ptr = self.log_cb.load(Ordering::Acquire);
        if cb_ptr.is_null() {
            return;
        }
        let cb: LogCallbackFn = unsafe { std::mem::transmute(cb_ptr) };
        let ctx_ptr = self.log_ctx.load(Ordering::Acquire);

        let entry = FfiLogEntry {
            level,
            plugin_id: FfiStr::from_str(self.plugin_id),
            target: FfiStr::from_str(event.metadata().target()),
            message: FfiStr::from_str(&message),
            timestamp_us: super::types::now_us(),
            instance_id: FfiStr::from_str(&instance_id),
            component_id: FfiStr::from_str(&component_id),
        };
        cb(ctx_ptr, &entry);
    }
}

/// Component routing info cached in span extensions.
#[derive(Clone)]
struct SpanComponentInfo {
    instance_id: String,
    component_id: String,
    component_type: String,
}

/// Visitor that extracts component routing fields from span attributes.
#[derive(Default)]
struct ComponentFieldVisitor {
    instance_id: Option<String>,
    component_id: Option<String>,
    component_type: Option<String>,
}

impl ComponentFieldVisitor {
    fn has_component_info(&self) -> bool {
        self.instance_id.is_some() || self.component_id.is_some()
    }
}

impl Visit for ComponentFieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let s = format!("{value:?}").trim_matches('"').to_string();
        match field.name() {
            "instance_id" => self.instance_id = Some(s),
            "component_id" => self.component_id = Some(s),
            "component_type" => self.component_type = Some(s),
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

/// Visitor that extracts the human-readable message from a tracing event.
#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}").trim_matches('"').to_string();
        } else if self.message.is_empty() {
            // Some events use the first field as the message
            self.message = format!("{value:?}").trim_matches('"').to_string();
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else if self.message.is_empty() {
            self.message = value.to_string();
        }
    }
}

/// Initialize the tracing subscriber with our FFI bridge layer.
///
/// Called from the `export_plugin!` macro during `__set_log_callback_impl`.
/// This sets up:
/// 1. A `LogTracer` that redirects all `log` crate events into `tracing`
/// 2. A `FfiTracingLayer` that captures tracing events (including those from `log`)
///    and forwards them through the FFI log callback with span context
pub fn init_tracing_subscriber(
    log_cb: &'static AtomicPtr<()>,
    log_ctx: &'static AtomicPtr<c_void>,
    plugin_id: &'static str,
) {
    use tracing_subscriber::layer::SubscriberExt;

    // Bridge log crate → tracing so that log::error!() etc. flow through
    // the tracing subscriber (which has span context for routing)
    let _ = tracing_log::LogTracer::init();

    let layer = FfiTracingLayer::new(log_cb, log_ctx, plugin_id);
    let subscriber = tracing_subscriber::registry().with(layer);

    // set_global_default only affects this cdylib's tracing dispatcher
    let _ = tracing::subscriber::set_global_default(subscriber);
}
