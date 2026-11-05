// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::telemetry::*;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fmt::{self, Write},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{
    span::{Attributes, Id, Record},
    subscriber::Interest,
    Event, Metadata, Subscriber,
};
use tracing_core::span::Current;

const DISCARDED: u64 = 1;
const MAX_ENTERED: usize = 4096;
static NEXT_ID: AtomicU64 = AtomicU64::new(2);

#[derive(Default)]
struct Counts {
    total: usize,
    bindings: BTreeMap<usize, usize>,
}
struct Slot {
    counts: Arc<Mutex<Counts>>,
    key: usize,
}
impl Drop for Slot {
    fn drop(&mut self) {
        match self.counts.lock() {
            Ok(mut counts) => {
                counts.total -= 1;
                if let Some(count) = counts.bindings.get_mut(&self.key) {
                    *count -= 1;
                    if *count == 0 {
                        counts.bindings.remove(&self.key);
                    }
                }
            }
            Err(error) => log::error!("Native span reservation poisoned: {error}"),
        }
    }
}
struct Entry {
    binding: PluginTelemetry,
    span: CompletedSpan,
    metadata: &'static Metadata<'static>,
    references: usize,
    remaining: usize,
    invalid: bool,
    sampling: Option<Arc<OnceLock<Result<RootSpanDecision>>>>,
    _slot: Slot,
}
#[derive(Default)]
struct State {
    entries: Mutex<BTreeMap<u64, Entry>>,
    counts: Arc<Mutex<Counts>>,
}
#[derive(Clone)]
struct Capture(Arc<State>);

fn capture() -> &'static Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE.get_or_init(|| Capture(Arc::new(State::default())))
}
pub(crate) fn dispatch() -> &'static tracing::Dispatch {
    static DISPATCH: OnceLock<tracing::Dispatch> = OnceLock::new();
    DISPATCH.get_or_init(|| tracing::Dispatch::new(capture().clone()))
}
pub(crate) fn diagnostics() -> &'static TelemetryDiagnostics {
    static DIAGNOSTICS: OnceLock<TelemetryDiagnostics> = OnceLock::new();
    DIAGNOSTICS.get_or_init(TelemetryDiagnostics::default)
}
pub(crate) fn unattributed() {
    diagnostics().record(TelemetryLoss::Unattributed);
}
pub(crate) fn loss_snapshot() -> TelemetryLossSnapshot {
    diagnostics().snapshot()
}

#[derive(Default)]
struct Entered {
    spans: Vec<u64>,
    overflow: usize,
}
thread_local! {
    static ENTERED: RefCell<Entered> = RefCell::new(Entered::default());
    static SAMPLING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn tracing_level(level: TelemetryLogLevel) -> tracing::Level {
    match level {
        TelemetryLogLevel::Trace => tracing::Level::TRACE,
        TelemetryLogLevel::Debug => tracing::Level::DEBUG,
        TelemetryLogLevel::Info => tracing::Level::INFO,
        TelemetryLogLevel::Warn => tracing::Level::WARN,
        TelemetryLogLevel::Error => tracing::Level::ERROR,
    }
}
fn level(level: &tracing::Level) -> TelemetryLogLevel {
    match *level {
        tracing::Level::TRACE => TelemetryLogLevel::Trace,
        tracing::Level::DEBUG => TelemetryLogLevel::Debug,
        tracing::Level::INFO => TelemetryLogLevel::Info,
        tracing::Level::WARN => TelemetryLogLevel::Warn,
        _ => TelemetryLogLevel::Error,
    }
}
fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|time| u64::try_from(time.as_nanos()).ok())
        .ok_or_else(|| {
            TelemetryError::Invalid(
                "native telemetry timestamp is outside its supported range".into(),
            )
        })
}
fn span_context(id: u64) -> ExecutionContext {
    let request = match capture().0.entries.lock() {
        Ok(entries) => entries.get(&id).map(|entry| {
            (
                entry.span.context.clone(),
                entry.sampling.as_ref().map(|sampling| {
                    (
                        sampling.clone(),
                        entry.binding.clone(),
                        root_request(&entry.span),
                    )
                }),
            )
        }),
        Err(_) => {
            diagnostics().record(TelemetryLoss::Capture);
            return ExecutionContext::default();
        }
    };
    let Some((context, pending)) = request else {
        return ExecutionContext::default();
    };
    let Some((sampling, binding, request)) = pending else {
        return ExecutionContext::new(context);
    };
    if SAMPLING.with(std::cell::Cell::get) {
        return ExecutionContext::new(context);
    }
    let decision = decide(&sampling, &binding, &request);
    match capture().0.entries.lock() {
        Ok(mut entries) => {
            if let Some(entry) = entries.get_mut(&id) {
                if entry.sampling.is_some() {
                    apply_decision(entry, decision);
                }
                return ExecutionContext::new(entry.span.context.clone());
            }
        }
        Err(_) => diagnostics().record(TelemetryLoss::Capture),
    }
    ExecutionContext::new(context)
}
fn root_request(span: &CompletedSpan) -> RootSpanRequest {
    RootSpanRequest {
        trace_id: span.context.trace_id(),
        name: span.name.clone(),
        target: span.target.clone(),
        attributes: span.attributes.clone(),
        links: span.links.clone(),
    }
}
fn decide(
    sampling: &OnceLock<Result<RootSpanDecision>>,
    binding: &PluginTelemetry,
    request: &RootSpanRequest,
) -> Result<RootSpanDecision> {
    sampling
        .get_or_init(|| {
            struct Reset;
            impl Drop for Reset {
                fn drop(&mut self) {
                    SAMPLING.with(|value| value.set(false));
                }
            }
            SAMPLING.with(|value| value.set(true));
            let _reset = Reset;
            binding.sample_root(request)
        })
        .clone()
}
fn apply_decision(entry: &mut Entry, decision: Result<RootSpanDecision>) {
    entry.sampling = None;
    match decision.and_then(|decision| {
        TraceContextV1::new(
            entry.span.context.trace_id(),
            entry.span.context.span_id(),
            u8::from(decision.sampled),
            decision.trace_state.as_deref().map(Into::into),
        )
    }) {
        Ok(context) => entry.span.context = context,
        Err(_) => {
            entry.invalid = true;
            entry.binding.diagnostics().record(TelemetryLoss::Capture);
        }
    }
}
fn current_context() -> ExecutionContext {
    let id = ENTERED.with(|entered| entered.borrow().spans.last().copied());
    id.map(span_context)
        .filter(|context| !context.is_empty())
        .unwrap_or_else(ExecutionScope::supplied_context)
}

struct Text {
    value: String,
    limit: usize,
}
impl Write for Text {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if value.len() > self.limit.saturating_sub(self.value.len()) {
            return Err(fmt::Error);
        }
        self.value.push_str(value);
        Ok(())
    }
}
fn formatted(arguments: fmt::Arguments<'_>, limit: usize) -> Option<String> {
    let mut text = Text {
        value: String::new(),
        limit,
    };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| text.write_fmt(arguments))) {
        Ok(Ok(())) => Some(text.value),
        _ => None,
    }
}

struct Fields<'a> {
    attributes: &'a mut TelemetryAttributes,
    remaining: &'a mut usize,
    invalid: &'a mut bool,
    status: &'a mut TelemetrySpanStatus,
    message: Option<String>,
    event: bool,
}
impl Fields<'_> {
    fn value(&mut self, name: &str, value: TelemetryAttribute) {
        if [
            "instance_id",
            "graph_id",
            "component_id",
            "component_kind",
            "plugin_kind",
            "generation",
            "role",
            "trace_id",
            "span_id",
        ]
        .contains(&name)
        {
            return; // Host identity replaces untrusted identity fields.
        }
        if name == "otel.status_code" {
            if let TelemetryAttribute::String(value) = &value {
                match value.as_str() {
                    "ERROR" | "Error" => *self.status = TelemetrySpanStatus::Error,
                    "OK" | "Ok" => *self.status = TelemetrySpanStatus::Ok,
                    _ => {}
                }
            }
            return;
        }
        let size = name.len()
            + 32
            + match &value {
                TelemetryAttribute::String(value) => value.len(),
                _ => 8,
            };
        if name.len() > 256
            || size > *self.remaining
            || matches!(&value, TelemetryAttribute::F64(value) if !value.is_finite())
            || matches!(&value, TelemetryAttribute::String(value) if value.len() > MAX_TELEMETRY_TEXT_BYTES || value.contains('\0'))
        {
            *self.invalid = true;
            return;
        }
        *self.remaining -= size;
        if self.event && name == "message" {
            self.message = match value {
                TelemetryAttribute::String(value) => Some(value),
                other => formatted(format_args!("{other:?}"), MAX_TELEMETRY_TEXT_BYTES),
            };
            return;
        }
        if let Some(existing) = self.attributes.iter_mut().find(|(key, _)| key == name) {
            existing.1 = value;
        } else if self.attributes.len() < MAX_TELEMETRY_ATTRIBUTES {
            self.attributes.push((name.into(), value));
        } else {
            *self.invalid = true;
        }
    }
}
impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        match formatted(
            format_args!("{value:?}"),
            MAX_TELEMETRY_TEXT_BYTES.min(*self.remaining),
        ) {
            Some(value) => self.value(field.name(), TelemetryAttribute::String(value)),
            None => *self.invalid = true,
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if value.len() > MAX_TELEMETRY_TEXT_BYTES.min(*self.remaining) {
            *self.invalid = true;
        } else {
            self.value(field.name(), TelemetryAttribute::String(value.into()));
        }
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.value(field.name(), TelemetryAttribute::I64(value));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.value(
            field.name(),
            i64::try_from(value)
                .map(TelemetryAttribute::I64)
                .unwrap_or_else(|_| TelemetryAttribute::String(value.to_string())),
        );
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.value(field.name(), TelemetryAttribute::Bool(value));
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.value(field.name(), TelemetryAttribute::F64(value));
    }
}

impl Capture {
    fn create(&self, attributes: &Attributes<'_>, binding: PluginTelemetry) -> Result<Id> {
        let key = binding.binding_key();
        let slot = {
            let mut counts = self.0.counts.lock().map_err(|_| TelemetryError::Poisoned)?;
            if counts.total >= MAX_NATIVE_OPEN_SPANS
                || counts.bindings.get(&key).copied().unwrap_or(0)
                    >= binding.config().limits.open_spans
            {
                return Err(TelemetryError::Capacity("open native spans"));
            }
            counts.total += 1;
            *counts.bindings.entry(key).or_default() += 1;
            Slot {
                counts: self.0.counts.clone(),
                key,
            }
        };
        let parent = match attributes.parent() {
            Some(parent) => span_context(parent.into_u64()),
            None if attributes.is_contextual() => current_context(),
            None => ExecutionContext::default(),
        };
        let mut fields = Vec::new();
        let mut remaining = binding.config().limits.record_bytes.saturating_sub(1024);
        let mut invalid = false;
        let mut status = TelemetrySpanStatus::Unset;
        if parent.trace().is_none_or(TraceContextV1::sampled) {
            attributes.record(&mut Fields {
                attributes: &mut fields,
                remaining: &mut remaining,
                invalid: &mut invalid,
                status: &mut status,
                message: None,
                event: false,
            });
        }
        let span_id: [u8; 8] = uuid::Uuid::new_v4().as_bytes()[..8]
            .try_into()
            .map_err(|_| TelemetryError::Invalid("invalid UUID prefix".into()))?;
        let root = parent.trace().is_none();
        let (trace_id, flags, state) = match parent.trace() {
            Some(parent) => (
                parent.trace_id(),
                parent.trace_flags(),
                parent.trace_state().map(Into::into),
            ),
            None => (*uuid::Uuid::new_v4().as_bytes(), 0, None),
        };
        let context = TraceContextV1::new(trace_id, span_id, flags, state)?;
        let id = NEXT_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| TelemetryError::Capacity("native span IDs"))?;
        let entry = Entry {
            span: CompletedSpan {
                name: attributes.metadata().name().into(),
                target: attributes.metadata().target().into(),
                context,
                parent_span_id: parent.trace().map(TraceContextV1::span_id),
                links: vec![],
                start_time_unix_ns: now()?,
                end_time_unix_ns: 0,
                status,
                attributes: fields,
                events: vec![],
            },
            binding,
            references: 1,
            metadata: attributes.metadata(),
            remaining,
            invalid,
            sampling: root.then(|| Arc::new(OnceLock::new())),
            _slot: slot,
        };
        self.0
            .entries
            .lock()
            .map_err(|_| TelemetryError::Poisoned)?
            .insert(id, entry);
        Ok(Id::from_u64(id))
    }

    fn event_record(&self, event: &Event<'_>) -> Result<()> {
        let parent_id = event.parent().map(Id::into_u64).or_else(|| {
            if event.is_contextual() {
                ENTERED.with(|value| value.borrow().spans.last().copied())
            } else {
                None
            }
        });
        let binding = match ExecutionScope::current_plugin_telemetry() {
            Some(binding) => Some(binding),
            None => match parent_id {
                Some(id) => self
                    .0
                    .entries
                    .lock()
                    .map_err(|_| TelemetryError::Poisoned)?
                    .get(&id)
                    .map(|entry| entry.binding.clone()),
                None => None,
            },
        };
        let Some(binding) = binding else {
            unattributed();
            return Ok(());
        };
        let timestamp = now()?;
        let mut attributes = Vec::new();
        let mut remaining = binding.config().limits.record_bytes.saturating_sub(1024);
        let mut invalid = false;
        let mut status = TelemetrySpanStatus::Unset;
        let mut visitor = Fields {
            attributes: &mut attributes,
            remaining: &mut remaining,
            invalid: &mut invalid,
            status: &mut status,
            message: None,
            event: true,
        };
        event.record(&mut visitor);
        let message = visitor
            .message
            .take()
            .unwrap_or_else(|| event.metadata().name().into());
        drop(visitor);
        if invalid {
            binding.diagnostics().record(TelemetryLoss::Capture);
            return Ok(());
        }
        let context = parent_id
            .map(span_context)
            .filter(|context| !context.is_empty())
            .unwrap_or_else(ExecutionScope::supplied_context);
        if let Some(id) = parent_id {
            let mut entries = self
                .0
                .entries
                .lock()
                .map_err(|_| TelemetryError::Poisoned)?;
            if let Some(entry) = entries
                .get_mut(&id)
                .filter(|entry| entry.span.context.sampled())
            {
                let size = binding
                    .config()
                    .limits
                    .record_bytes
                    .saturating_sub(remaining)
                    + message.len();
                if entry.span.events.len() >= MAX_TELEMETRY_EVENTS || size > entry.remaining {
                    entry.invalid = true;
                } else {
                    entry.remaining -= size;
                    let mut event_attributes = attributes.clone();
                    event_attributes.push((
                        "message".into(),
                        TelemetryAttribute::String(message.clone()),
                    ));
                    entry.span.events.push(TelemetrySpanEvent {
                        name: event.metadata().name().into(),
                        time_unix_ns: timestamp,
                        attributes: event_attributes,
                    });
                    if *event.metadata().level() == tracing::Level::ERROR {
                        entry.span.status = TelemetrySpanStatus::Error;
                    }
                }
            }
        }
        if binding.enabled(
            event.metadata().target(),
            level(event.metadata().level()),
            false,
        ) {
            binding.submit_log(&TelemetryLogRecord {
                time_unix_ns: timestamp,
                level: level(event.metadata().level()),
                target: event.metadata().target().into(),
                message,
                attributes,
                context: context.trace().cloned(),
            });
        }
        Ok(())
    }
}

impl Subscriber for Capture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        if SAMPLING.with(std::cell::Cell::get) && metadata.is_span() {
            return false;
        }
        let bound = ExecutionScope::current_plugin_telemetry();
        bound.is_some_and(|binding| {
            binding.enabled(
                metadata.target(),
                level(metadata.level()),
                metadata.is_span(),
            ) || (metadata.is_event() && binding.config().tracing)
        })
    }
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::TRACE)
    }
    fn new_span(&self, attributes: &Attributes<'_>) -> Id {
        let Some(binding) = ExecutionScope::current_plugin_telemetry() else {
            unattributed();
            return Id::from_u64(DISCARDED);
        };
        match self.create(attributes, binding.clone()) {
            Ok(id) => id,
            Err(_) => {
                binding.diagnostics().record(TelemetryLoss::Capture);
                Id::from_u64(DISCARDED)
            }
        }
    }
    fn record(&self, id: &Id, values: &Record<'_>) {
        let limits = match self.0.entries.lock() {
            Ok(entries) => entries
                .get(&id.into_u64())
                .filter(|entry| entry.span.context.sampled() || entry.sampling.is_some())
                .map(|entry| entry.remaining),
            Err(_) => {
                diagnostics().record(TelemetryLoss::Capture);
                return;
            }
        };
        let Some(mut remaining) = limits else {
            return;
        };
        let mut attributes = Vec::new();
        let mut invalid = false;
        let mut status = TelemetrySpanStatus::Unset;
        // A user Debug implementation may emit tracing itself. Never invoke it
        // while holding the span registry lock.
        values.record(&mut Fields {
            attributes: &mut attributes,
            remaining: &mut remaining,
            invalid: &mut invalid,
            status: &mut status,
            message: None,
            event: false,
        });
        match self.0.entries.lock() {
            Ok(mut entries) => {
                if let Some(entry) = entries.get_mut(&id.into_u64()) {
                    entry.invalid |= invalid;
                    let mut fields = Fields {
                        attributes: &mut entry.span.attributes,
                        remaining: &mut entry.remaining,
                        invalid: &mut entry.invalid,
                        status: &mut entry.span.status,
                        message: None,
                        event: false,
                    };
                    for (key, value) in attributes {
                        fields.value(&key, value);
                    }
                    if status != TelemetrySpanStatus::Unset {
                        entry.span.status = status;
                    }
                }
            }
            Err(_) => diagnostics().record(TelemetryLoss::Capture),
        }
    }
    fn record_follows_from(&self, id: &Id, follows: &Id) {
        let context = span_context(follows.into_u64());
        if let Some(context) = context.trace() {
            if let Err(error) = link(id, context) {
                log::debug!("Native span link rejected: {error}");
            }
        }
    }
    fn event(&self, event: &Event<'_>) {
        if self.event_record(event).is_err() {
            diagnostics().record(TelemetryLoss::Capture);
        }
    }
    fn enter(&self, id: &Id) {
        if id.into_u64() == DISCARDED {
            return;
        }
        let _context = span_context(id.into_u64());
        ENTERED.with(|entered| {
            let mut entered = entered.borrow_mut();
            if entered.spans.len() == MAX_ENTERED || entered.overflow != 0 {
                entered.overflow = entered.overflow.saturating_add(1);
                diagnostics().record(TelemetryLoss::Capture);
            } else {
                entered.spans.push(id.into_u64());
            }
        });
    }
    fn exit(&self, id: &Id) {
        if id.into_u64() == DISCARDED {
            return;
        }
        ENTERED.with(|entered| {
            let mut entered = entered.borrow_mut();
            if entered.overflow != 0 {
                entered.overflow -= 1;
            } else if let Some(index) = entered
                .spans
                .iter()
                .rposition(|value| *value == id.into_u64())
            {
                entered.spans.remove(index);
            }
        });
    }
    fn clone_span(&self, id: &Id) -> Id {
        if let Ok(mut entries) = self.0.entries.lock() {
            if let Some(entry) = entries.get_mut(&id.into_u64()) {
                entry.references = entry.references.saturating_add(1);
            }
        } else {
            diagnostics().record(TelemetryLoss::Capture);
        }
        id.clone()
    }
    fn try_close(&self, id: Id) -> bool {
        let entry = {
            let Ok(mut entries) = self.0.entries.lock() else {
                diagnostics().record(TelemetryLoss::Capture);
                return false;
            };
            let Some(entry) = entries.get_mut(&id.into_u64()) else {
                return true;
            };
            entry.references -= 1;
            if entry.references != 0 {
                return false;
            }
            entries.remove(&id.into_u64())
        };
        if let Some(mut entry) = entry {
            if let Some(sampling) = entry.sampling.clone() {
                let decision = decide(&sampling, &entry.binding, &root_request(&entry.span));
                apply_decision(&mut entry, decision);
            }
            if entry.span.context.sampled() {
                match now() {
                    Ok(end) if !entry.invalid => {
                        entry.span.end_time_unix_ns = end;
                        entry.binding.submit_span(&entry.span);
                    }
                    _ => entry.binding.diagnostics().record(TelemetryLoss::Capture),
                }
            }
        }
        true
    }
    fn current_span(&self) -> Current {
        let id = ENTERED.with(|entered| entered.borrow().spans.last().copied());
        match self.0.entries.lock() {
            Ok(entries) => id
                .and_then(|id| {
                    entries
                        .get(&id)
                        .map(|entry| Current::new(Id::from_u64(id), entry.metadata))
                })
                .unwrap_or_else(Current::none),
            Err(_) => {
                diagnostics().record(TelemetryLoss::Capture);
                Current::none()
            }
        }
    }
}

fn link(id: &Id, context: &TraceContextV1) -> Result<()> {
    let mut entries = capture()
        .0
        .entries
        .lock()
        .map_err(|_| TelemetryError::Poisoned)?;
    let Some(entry) = entries.get_mut(&id.into_u64()) else {
        return Ok(());
    };
    let size = 128 + context.trace_state().map_or(0, str::len);
    if entry.span.links.len() >= MAX_TELEMETRY_LINKS || size > entry.remaining {
        entry.invalid = true;
        return Err(TelemetryError::Capacity("native span links"));
    }
    entry.remaining -= size;
    entry.span.links.push(TelemetrySpanLink {
        context: context.clone(),
        attributes: vec![],
    });
    Ok(())
}

#[derive(Debug)]
pub(crate) struct NativeContextAdapter;
struct Guard;
impl TraceContextGuard for Guard {}
impl TraceContextAdapter for NativeContextAdapter {
    fn enter(&self, _: &ExecutionContext) -> Box<dyn TraceContextGuard> {
        Box::new(Guard)
    }
    fn current_context(&self) -> ExecutionContext {
        current_context()
    }
    fn span_context(&self, span: &tracing::Span) -> ExecutionContext {
        span.with_subscriber(|(id, dispatch)| {
            if dispatch.downcast_ref::<Capture>().is_some() {
                span_context(id.into_u64())
            } else {
                ExecutionContext::default()
            }
        })
        .unwrap_or_default()
    }
    fn set_parent(&self, span: &tracing::Span, parent: &ExecutionContext) -> Result<()> {
        if let (Some(id), Some(parent)) = (span.id(), parent.trace()) {
            let mut entries = capture()
                .0
                .entries
                .lock()
                .map_err(|_| TelemetryError::Poisoned)?;
            if let Some(entry) = entries.get_mut(&id.into_u64()) {
                entry.span.parent_span_id = Some(parent.span_id());
                entry.span.context = TraceContextV1::new(
                    parent.trace_id(),
                    entry.span.context.span_id(),
                    parent.trace_flags(),
                    parent.trace_state().map(Into::into),
                )?;
            }
        }
        Ok(())
    }
    fn add_link(&self, span: &tracing::Span, context: &TraceContextV1) -> Result<()> {
        match span.id() {
            Some(id) => link(&id, context),
            None => Ok(()),
        }
    }
}

pub(crate) struct NativeLogger;
pub(crate) static LOGGER: NativeLogger = NativeLogger;
impl log::Log for NativeLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        let level = match metadata.level() {
            log::Level::Trace => TelemetryLogLevel::Trace,
            log::Level::Debug => TelemetryLogLevel::Debug,
            log::Level::Info => TelemetryLogLevel::Info,
            log::Level::Warn => TelemetryLogLevel::Warn,
            log::Level::Error => TelemetryLogLevel::Error,
        };
        ExecutionScope::current_plugin_telemetry()
            .is_none_or(|binding| binding.enabled(metadata.target(), level, false))
    }

    fn log(&self, record: &log::Record<'_>) {
        let Some(binding) = ExecutionScope::current_plugin_telemetry() else {
            if record.target() == "drasi::telemetry_loss" {
                if let Some(message) = formatted(*record.args(), MAX_TELEMETRY_TEXT_BYTES) {
                    eprintln!("{message}");
                }
            } else {
                unattributed();
            }
            return;
        };
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = match record.level() {
            log::Level::Trace => TelemetryLogLevel::Trace,
            log::Level::Debug => TelemetryLogLevel::Debug,
            log::Level::Info => TelemetryLogLevel::Info,
            log::Level::Warn => TelemetryLogLevel::Warn,
            log::Level::Error => TelemetryLogLevel::Error,
        };
        match (now(), formatted(*record.args(), MAX_TELEMETRY_TEXT_BYTES)) {
            (Ok(time_unix_ns), Some(message)) => {
                binding.submit_log(&TelemetryLogRecord {
                    time_unix_ns,
                    level,
                    target: record.target().into(),
                    message,
                    attributes: vec![],
                    context: current_context().trace().cloned(),
                });
            }
            _ => binding.diagnostics().record(TelemetryLoss::Capture),
        }
    }
    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{ComponentId, ComponentRole, ComponentSemanticKind};
    use metrics::Counter;

    #[derive(Debug)]
    struct Backend {
        config: PluginTelemetryConfig,
        diagnostics: TelemetryDiagnostics,
        spans: Mutex<Vec<CompletedSpan>>,
        roots: AtomicU64,
    }
    impl PluginTelemetryBackend for Backend {
        fn config(&self) -> &PluginTelemetryConfig {
            &self.config
        }
        fn diagnostics(&self) -> &TelemetryDiagnostics {
            &self.diagnostics
        }
        fn for_plugin(&self, _: &str) -> Result<PluginTelemetry> {
            Err(TelemetryError::Unsupported("test rebind"))
        }
        fn register_metric(&self, _: &PluginMetricRegistration) -> Result<RegisteredMetric> {
            Ok(RegisteredMetric::Counter(Counter::noop()))
        }
        fn register_metric_handle(
            &self,
            _: &PluginMetricRegistration,
        ) -> Result<Option<PluginMetricHandle>> {
            Err(TelemetryError::Unsupported("test handles"))
        }
        fn update_metric(&self, _: PluginMetricUpdate) -> Result<()> {
            Err(TelemetryError::Unsupported("test updates"))
        }
        fn has_span_collector(&self) -> bool {
            true
        }
        fn submit_encoded_span(&self, _: &[u8]) -> TelemetrySubmission {
            TelemetrySubmission::Invalid
        }
        fn submit_span(&self, span: &CompletedSpan) -> TelemetrySubmission {
            span.validate().unwrap();
            self.spans.lock().unwrap().push(span.clone());
            TelemetrySubmission::Accepted
        }
        fn submit_log(&self, _: &TelemetryLogRecord) -> TelemetrySubmission {
            TelemetrySubmission::Accepted
        }
        fn sample_root(&self, request: &RootSpanRequest) -> Result<RootSpanDecision> {
            request.validate()?;
            self.roots.fetch_add(1, Ordering::SeqCst);
            Ok(RootSpanDecision {
                sampled: true,
                trace_state: Some("test=state".into()),
            })
        }
    }

    fn setup(open_spans: usize) -> (Arc<Backend>, ExecutionScope) {
        let backend = Arc::new(Backend {
            config: PluginTelemetryConfig {
                scope: TelemetryScope::graph("capture", "graph")
                    .unwrap()
                    .component(
                        ComponentId::try_new("probe").unwrap(),
                        ComponentRole::Service,
                        ComponentSemanticKind::Service,
                    ),
                generation: 1,
                plugin_kind: "test".into(),
                tracing: true,
                metrics: false,
                native_metrics_required: false,
                trace_directives: "trace".into(),
                log_directives: "off".into(),
                metric_exclusions: vec![],
                sampling: PluginRootSampling::Always,
                limits: TelemetryLimits {
                    open_spans,
                    ..Default::default()
                },
            },
            diagnostics: Default::default(),
            spans: Mutex::new(vec![]),
            roots: AtomicU64::new(0),
        });
        let _dispatch = tracing::dispatcher::set_default(dispatch());
        let scope = ExecutionScope::new(Default::default())
            .with_component(backend.config.scope.clone(), 1)
            .with_plugin_telemetry(Some(PluginTelemetry::new(backend.clone())))
            .with_adapter(Some(Arc::new(NativeContextAdapter)));
        (backend, scope)
    }

    #[test]
    fn root_sampling_waits_for_links_fields_and_runs_once_even_with_reentrant_debug() {
        let (backend, scope) = setup(8);
        scope.in_scope(|| {
            struct Reentrant;
            impl fmt::Debug for Reentrant {
                fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    tracing::info!("recording nested diagnostic");
                    formatter.write_str("safe")
                }
            }
            let span = tracing::info_span!("capture.root", value = tracing::field::Empty);
            span.record("value", tracing::field::debug(Reentrant));
            let link_context = TraceContextV1::new([7; 16], [8; 8], 1, None).unwrap();
            NativeContextAdapter.add_link(&span, &link_context).unwrap();
            assert_eq!(backend.roots.load(Ordering::SeqCst), 0);
            let original = NativeContextAdapter.span_context(&span);
            assert_eq!(NativeContextAdapter.span_context(&span), original);
            span.in_scope(|| {
                assert_eq!(ExecutionContext::current(), original);
                tracing::info!(count = 3, "work done");
            });
            drop(span);
            assert_eq!(backend.roots.load(Ordering::SeqCst), 1);
            let records = backend.spans.lock().unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].context, original.trace().unwrap().clone());
            assert_eq!(records[0].links[0].context, link_context);
            assert_eq!(records[0].context.trace_state(), Some("test=state"));
            assert_eq!(
                records[0].attributes,
                vec![("value".into(), TelemetryAttribute::String("safe".into()))]
            );
            assert_eq!(records[0].events.len(), 1);
            assert_eq!(backend.diagnostics.snapshot().capture, 0);
        });
    }

    #[test]
    fn open_span_and_field_limits_reject_without_unbounded_state_or_fallback_queue() {
        let (backend, scope) = setup(1);
        scope.in_scope(|| {
            let first = tracing::info_span!("bounded.first");
            let rejected = tracing::info_span!("bounded.second");
            assert_eq!(rejected.id().unwrap().into_u64(), DISCARDED);
            drop(rejected);
            drop(first);
            let oversized = tracing::info_span!("bounded.fields", value = tracing::field::Empty);
            oversized.record("value", "x".repeat(MAX_TELEMETRY_TEXT_BYTES + 1));
            drop(oversized);
            assert_eq!(backend.spans.lock().unwrap().len(), 1);
            assert_eq!(backend.diagnostics.snapshot().capture, 2);
            let remaining = capture().0.counts.lock().unwrap();
            assert!(!remaining.bindings.contains_key(&scope.in_scope(|| {
                ExecutionScope::current_plugin_telemetry()
                    .unwrap()
                    .binding_key()
            })));
        });
    }
}
