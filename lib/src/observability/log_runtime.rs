// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Owned component-log capture and the process-scoped default logging service.

use super::{ComponentLogKey, ComponentLogLayer, ComponentLogRegistry, LogLevel, LogMessage};
use crate::context::workers::{
    join_owned_worker_gracefully, spawn_owned_blocking_worker, WorkerCleanupError, WorkerCompletion,
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch, RwLock};
use tracing_subscriber::{filter::Targets, prelude::*, EnvFilter};

type FilterError = Arc<dyn std::error::Error + Send + Sync>;

pub const DEFAULT_CAPTURE_CAPACITY: usize = 10_000;
pub const PLUGIN_LOG_TARGET: &str = "drasi_plugin";

#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum LoggingError {
    #[error("invalid logging configuration: {0}")]
    Configuration(String),
    #[error("legacy plugin log capture requires static target/level directives: {0}")]
    LegacyFilter(#[source] FilterError),
    #[error("legacy callback records do not carry the host span context required by span/field directives")]
    LegacySpanContext,
    #[error("cannot create the component-log runtime: {0}")]
    Startup(#[source] Arc<std::io::Error>),
    #[error("a host logger is already installed; supply a composed ComponentLogRuntime instead of automatic setup")]
    LoggerConflict,
    #[error("a host subscriber is already installed; supply a composed ComponentLogRuntime (the log bridge may already be installed)")]
    SubscriberConflict,
    #[error("component logging is shut down; the process-global subscriber cannot be reinstalled")]
    Closed,
    #[error("component-log worker panicked")]
    WorkerPanic,
    #[error("component-log cleanup was cancelled; retry shutdown to join the retained worker")]
    CleanupCancelled,
    #[error("component-log cleanup failed: {0}")]
    Cleanup(#[source] Arc<WorkerCleanupError>),
    #[error("component-log shutdown exceeded {0:?}; cleanup ownership is retained")]
    TimedOut(Duration),
    #[error("component-log ownership lock is poisoned")]
    Poisoned,
}

pub type LoggingResult<T> = std::result::Result<T, LoggingError>;

/// Log-only filtering. Its forwarding ceiling is independent of every tracing
/// layer. Legacy callbacks support target/level filters but cannot reconstruct
/// arbitrary host span/field filters.
#[derive(Clone, Debug)]
pub struct LogPolicy {
    filter: EnvFilter,
    maximum: log::LevelFilter,
    legacy_targets: std::result::Result<Targets, FilterError>,
}

impl LogPolicy {
    pub fn new(directives: &str) -> LoggingResult<Self> {
        let filter = EnvFilter::try_new(directives)
            .map_err(|error| LoggingError::Configuration(error.to_string()))?;
        let maximum = match filter.max_level_hint() {
            Some(tracing::level_filters::LevelFilter::OFF) => log::LevelFilter::Off,
            Some(tracing::level_filters::LevelFilter::ERROR) => log::LevelFilter::Error,
            Some(tracing::level_filters::LevelFilter::WARN) => log::LevelFilter::Warn,
            Some(tracing::level_filters::LevelFilter::INFO) => log::LevelFilter::Info,
            Some(tracing::level_filters::LevelFilter::DEBUG) => log::LevelFilter::Debug,
            _ => log::LevelFilter::Trace,
        };
        let canonical = filter.to_string();
        let legacy_targets = if canonical.contains('[') {
            let error: FilterError = Arc::new(LoggingError::LegacySpanContext);
            Err(error)
        } else {
            canonical
                .parse::<Targets>()
                .map_err(|error| -> FilterError { Arc::new(error) })
        };
        Ok(Self {
            filter,
            maximum,
            legacy_targets,
        })
    }

    pub fn from_env() -> LoggingResult<Self> {
        match std::env::var("RUST_LOG") {
            Ok(value) => Self::new(&value),
            Err(std::env::VarError::NotPresent) => Self::new("info"),
            Err(error) => Err(LoggingError::Configuration(error.to_string())),
        }
    }

    pub fn max_level(&self) -> log::LevelFilter {
        self.maximum
    }

    pub fn legacy_max_level(&self) -> LoggingResult<log::LevelFilter> {
        self.legacy_targets
            .as_ref()
            .map_err(|error| LoggingError::LegacyFilter(error.clone()))?;
        Ok(self.maximum)
    }

    fn allows_target(&self, target: &str, level: LogLevel) -> LoggingResult<bool> {
        let targets = self
            .legacy_targets
            .as_ref()
            .map_err(|error| LoggingError::LegacyFilter(error.clone()))?;
        let level = match level {
            LogLevel::Error => tracing::Level::ERROR,
            LogLevel::Warn => tracing::Level::WARN,
            LogLevel::Info => tracing::Level::INFO,
            LogLevel::Debug => tracing::Level::DEBUG,
            LogLevel::Trace => tracing::Level::TRACE,
        };
        Ok(targets.would_enable(target, &level))
    }

    pub(crate) fn filter(&self) -> EnvFilter {
        self.filter.clone()
    }

    fn allows(&self, level: LogLevel) -> bool {
        let level = match level {
            LogLevel::Error => log::Level::Error,
            LogLevel::Warn => log::Level::Warn,
            LogLevel::Info => log::Level::Info,
            LogLevel::Debug => log::Level::Debug,
            LogLevel::Trace => log::Level::Trace,
        };
        level <= self.maximum
    }
}

enum Command {
    Log(LogMessage),
    Flush(oneshot::Sender<()>),
    Remove(ComponentLogKey, oneshot::Sender<()>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogCaptureLosses {
    pub full: u64,
    pub closed: u64,
}

pub(super) struct RuntimeInner {
    registry: Arc<ComponentLogRegistry>,
    policy: LogPolicy,
    sender: mpsc::Sender<Command>,
    shutdown: watch::Sender<bool>,
    open: AtomicBool,
    full: AtomicU64,
    closed: AtomicU64,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    join: RwLock<Option<tokio::task::JoinHandle<LoggingResult<()>>>>,
    cleanup: tokio::sync::Mutex<()>,
    terminal: Mutex<Option<LoggingResult<()>>>,
}

impl RuntimeInner {
    pub(super) fn max_level(&self) -> log::LevelFilter {
        self.policy.max_level()
    }

    pub(super) fn legacy_max_level(&self) -> LoggingResult<log::LevelFilter> {
        if !self.open.load(Ordering::Acquire) || self.sender.is_closed() {
            return Err(LoggingError::Closed);
        }
        self.policy.legacy_max_level()
    }

    pub(super) fn submit_target(&self, target: &str, message: LogMessage) -> LoggingResult<bool> {
        if !self.policy.allows_target(target, message.level)? {
            return Ok(true);
        }
        Ok(self.submit(message))
    }

    pub(super) fn submit(&self, message: LogMessage) -> bool {
        self.submit_outcome(message) == super::TelemetrySubmission::Accepted
    }

    fn submit_outcome(&self, message: LogMessage) -> super::TelemetrySubmission {
        use super::TelemetrySubmission;
        if !self.policy.allows(message.level) {
            return TelemetrySubmission::Accepted;
        }
        if !self.open.load(Ordering::Acquire) {
            loss(&self.closed, "closed");
            return TelemetrySubmission::Closed;
        }
        match self.sender.try_send(Command::Log(message)) {
            Ok(()) => TelemetrySubmission::Accepted,
            Err(mpsc::error::TrySendError::Full(_)) => {
                loss(&self.full, "full");
                TelemetrySubmission::Full
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                loss(&self.closed, "closed");
                TelemetrySubmission::Closed
            }
        }
    }

    fn close(&self) {
        self.open.store(false, Ordering::Release);
        self.shutdown.send_replace(true);
    }
}

impl Drop for RuntimeInner {
    fn drop(&mut self) {
        self.close();
        match self.thread.get_mut() {
            Ok(Some(_)) => eprintln!(
                "drasi-lib: component logging dropped without awaited shutdown; drain requested"
            ),
            Err(_) => {
                eprintln!("drasi-lib: logging ownership poisoned during drop; drain requested")
            }
            Ok(None) => {}
        }
    }
}

fn loss(counter: &AtomicU64, reason: &str) {
    let count = counter.fetch_add(1, Ordering::Relaxed).saturating_add(1);
    if count.is_power_of_two() {
        eprintln!("drasi-lib: component log transport {reason}; {count} records lost");
    }
}

/// Cloneable ownership of one capture worker. Layers retain the same owner.
/// Call `shutdown` to drain and join; a cancelled/timed-out wait may be retried.
#[derive(Clone)]
pub struct ComponentLogRuntime {
    inner: Arc<RuntimeInner>,
}

impl std::fmt::Debug for ComponentLogRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ComponentLogRuntime")
            .field("open", &self.inner.open.load(Ordering::Acquire))
            .field("policy", &self.inner.policy)
            .finish()
    }
}

impl ComponentLogRuntime {
    pub(super) fn from_inner(inner: Arc<RuntimeInner>) -> Self {
        Self { inner }
    }

    pub fn new(policy: LogPolicy) -> LoggingResult<Self> {
        Self::with_capacity(policy, DEFAULT_CAPTURE_CAPACITY)
    }

    pub fn with_capacity(policy: LogPolicy, capacity: usize) -> LoggingResult<Self> {
        Self::for_registry(Arc::new(ComponentLogRegistry::new()), policy, capacity)
    }

    fn for_registry(
        registry: Arc<ComponentLogRegistry>,
        policy: LogPolicy,
        capacity: usize,
    ) -> LoggingResult<Self> {
        if capacity == 0 {
            return Err(LoggingError::Configuration(
                "capture capacity must be positive".into(),
            ));
        }
        let (sender, mut receiver) = mpsc::channel(capacity);
        let (shutdown, mut closing) = watch::channel(false);
        let (ready, startup) = std::sync::mpsc::sync_channel(1);
        let inner = Arc::new(RuntimeInner {
            registry: registry.clone(),
            policy,
            sender,
            shutdown,
            open: AtomicBool::new(true),
            full: AtomicU64::new(0),
            closed: AtomicU64::new(0),
            thread: Mutex::new(None),
            join: RwLock::new(None),
            cleanup: tokio::sync::Mutex::new(()),
            terminal: Mutex::new(None),
        });
        let mut owner = inner.thread.lock().map_err(|_| LoggingError::Poisoned)?;
        *owner = Some(
            std::thread::Builder::new()
                .name("drasi-log-worker".into())
                .spawn(move || {
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            let _ = ready.send(Err(LoggingError::Startup(Arc::new(error))));
                            return;
                        }
                    };
                    if ready.send(Ok(())).is_err() {
                        return;
                    }
                    runtime.block_on(async move {
                        loop {
                            let command = tokio::select! {
                                biased;
                                _ = closing.changed() => {
                                    receiver.close();
                                    break;
                                }
                                command = receiver.recv() => command,
                            };
                            let Some(command) = command else { break };
                            record(&registry, command).await;
                        }
                        while let Some(command) = receiver.recv().await {
                            record(&registry, command).await;
                        }
                    });
                })
                .map_err(|error| LoggingError::Startup(Arc::new(error)))?,
        );
        drop(owner);
        let capture = Self { inner };
        let started = startup.recv().unwrap_or(Err(LoggingError::WorkerPanic));
        if let Err(error) = started {
            capture.discard_uninstalled()?;
            return Err(error);
        }
        if let Err(error) = capture
            .inner
            .registry
            .bind_capture(Arc::downgrade(&capture.inner))
        {
            capture.discard_uninstalled()?;
            return Err(error);
        }
        Ok(capture)
    }

    pub fn registry(&self) -> Arc<ComponentLogRegistry> {
        self.inner.registry.clone()
    }

    pub fn layer<S>(&self) -> impl tracing_subscriber::Layer<S> + Send + Sync + 'static
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        ComponentLogLayer::new(self.clone()).with_filter(super::tracing_layer::ComponentLogFilter(
            self.policy().filter(),
        ))
    }

    pub fn policy(&self) -> &LogPolicy {
        &self.inner.policy
    }

    pub fn is_closed(&self) -> bool {
        !self.inner.open.load(Ordering::Acquire) || self.inner.sender.is_closed()
    }

    pub fn losses(&self) -> LogCaptureLosses {
        LogCaptureLosses {
            full: self.inner.full.load(Ordering::Relaxed),
            closed: self.inner.closed.load(Ordering::Relaxed),
        }
    }

    pub fn try_log(&self, message: LogMessage) -> bool {
        self.inner.submit(message)
    }

    pub(crate) fn try_native_log(
        &self,
        target: &str,
        message: LogMessage,
    ) -> LoggingResult<super::TelemetrySubmission> {
        if !self.inner.policy.allows_target(target, message.level)? {
            return Ok(super::TelemetrySubmission::Accepted);
        }
        Ok(self.inner.submit_outcome(message))
    }

    pub async fn flush(&self) -> LoggingResult<()> {
        if self.is_closed() {
            return Err(LoggingError::Closed);
        }
        let (reply, completed) = oneshot::channel();
        self.inner
            .sender
            .send(Command::Flush(reply))
            .await
            .map_err(|_| LoggingError::Closed)?;
        completed.await.map_err(|_| LoggingError::Closed)
    }

    pub(super) async fn remove_history(&self, key: &ComponentLogKey) -> LoggingResult<()> {
        if !self.is_closed() {
            let (reply, completed) = oneshot::channel();
            if self
                .inner
                .sender
                .send(Command::Remove(key.clone(), reply))
                .await
                .is_ok()
            {
                return completed.await.map_err(|_| LoggingError::Closed);
            }
        }
        self.shutdown(Duration::from_secs(2)).await?;
        self.inner.registry.remove_stored(key).await;
        Ok(())
    }

    pub async fn shutdown(&self, timeout: Duration) -> LoggingResult<()> {
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| LoggingError::Configuration("invalid shutdown timeout".into()))?;
        self.inner.close();
        let _cleanup = tokio::time::timeout_at(deadline, self.inner.cleanup.lock())
            .await
            .map_err(|_| LoggingError::TimedOut(timeout))?;
        if let Some(result) = self
            .inner
            .terminal
            .lock()
            .map_err(|_| LoggingError::Poisoned)?
            .clone()
        {
            return result;
        }
        if self.inner.join.read().await.is_none() {
            let inner = self.inner.clone();
            spawn_owned_blocking_worker(&self.inner.join, move || {
                let thread = inner
                    .thread
                    .lock()
                    .map_err(|_| LoggingError::Poisoned)?
                    .take();
                let thread = thread.ok_or_else(|| {
                    LoggingError::Configuration("capture worker owner missing".into())
                })?;
                thread.join().map_err(|_| LoggingError::WorkerPanic)?;
                Ok(())
            })
            .await
            .map_err(|_| LoggingError::Configuration("cleanup already owned".into()))?;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let result =
            join_owned_worker_gracefully(&mut *self.inner.join.write().await, remaining).await;
        let result = match result {
            Ok(WorkerCompletion::Completed(result)) => result,
            Ok(WorkerCompletion::Cancelled) => return Err(LoggingError::CleanupCancelled),
            Ok(WorkerCompletion::Absent) => Err(LoggingError::Configuration(
                "capture join owner missing".into(),
            )),
            Err(error @ WorkerCleanupError::TimedOut { .. }) => {
                return Err(LoggingError::Cleanup(Arc::new(error)));
            }
            Err(error) => Err(LoggingError::Cleanup(Arc::new(error))),
        };
        *self
            .inner
            .terminal
            .lock()
            .map_err(|_| LoggingError::Poisoned)? = Some(result.clone());
        result
    }

    fn discard_uninstalled(&self) -> LoggingResult<()> {
        self.inner.close();
        let thread = self
            .inner
            .thread
            .lock()
            .map_err(|_| LoggingError::Poisoned)?
            .take();
        if let Some(thread) = thread {
            thread.join().map_err(|_| LoggingError::WorkerPanic)?;
        }
        Ok(())
    }
}

async fn record(registry: &ComponentLogRegistry, command: Command) {
    match command {
        Command::Log(message) => registry.log(message).await,
        Command::Flush(reply) => {
            let _ = reply.send(());
        }
        Command::Remove(key, reply) => {
            registry.remove_stored(&key).await;
            let _ = reply.send(());
        }
    }
}

static GLOBAL_LOG_REGISTRY: OnceLock<Arc<ComponentLogRegistry>> = OnceLock::new();
static DEFAULT_LOGGING: Mutex<Option<LoggingResult<ComponentLogRuntime>>> = Mutex::new(None);

/// Passive compatibility accessor. This no longer installs logging; plain
/// library construction still initializes the default service automatically.
pub fn get_or_init_global_registry() -> Arc<ComponentLogRegistry> {
    GLOBAL_LOG_REGISTRY
        .get_or_init(|| Arc::new(ComponentLogRegistry::new()))
        .clone()
}

/// Creates capture only. The caller composes its layer and owns shutdown.
pub fn init_component_logging() -> LoggingResult<ComponentLogRuntime> {
    ComponentLogRuntime::new(LogPolicy::from_env()?)
}

/// Installs or borrows the known process-wide default logging service.
/// Does not install a metric recorder or an exporter. Shutdown is terminal.
pub fn init_default_subscriber() -> LoggingResult<ComponentLogRuntime> {
    let mut default = DEFAULT_LOGGING.lock().map_err(|_| LoggingError::Poisoned)?;
    if let Some(result) = default.as_ref() {
        let runtime = result.clone()?;
        return if runtime.is_closed() {
            Err(LoggingError::Closed)
        } else {
            Ok(runtime)
        };
    }
    let runtime = ComponentLogRuntime::for_registry(
        get_or_init_global_registry(),
        LogPolicy::from_env()?,
        DEFAULT_CAPTURE_CAPACITY,
    )?;
    let installed = (|| {
        tracing_log::LogTracer::init().map_err(|_| LoggingError::LoggerConflict)?;
        let subscriber = tracing_subscriber::registry()
            .with(runtime.layer())
            .with(tracing_subscriber::fmt::layer().with_filter(runtime.policy().filter()));
        tracing::subscriber::set_global_default(subscriber)
            .map_err(|_| LoggingError::SubscriberConflict)?;
        Ok(runtime.clone())
    })();
    if installed.is_err() {
        runtime.discard_uninstalled()?;
    }
    *default = Some(installed.clone());
    installed
}

/// The known default service's log-only policy, not the subscriber's span level.
pub fn default_log_level() -> LoggingResult<Option<log::LevelFilter>> {
    let state = DEFAULT_LOGGING.lock().map_err(|_| LoggingError::Poisoned)?;
    match state.as_ref() {
        Some(Ok(runtime)) if runtime.is_closed() => Err(LoggingError::Closed),
        Some(Ok(runtime)) => Ok(Some(runtime.policy().max_level())),
        Some(Err(error)) => Err(error.clone()),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::{ComponentLogKey, ComponentLogType};
    use tracing_subscriber::filter::filter_fn;

    fn emit(instance: &str) {
        let span = tracing::info_span!(
            "computation_component",
            instance_id = instance,
            component_id = "component",
            component_type = "source"
        );
        let _entered = span.enter();
        tracing::info!("visible info");
        tracing::debug!("debug detail");
        tracing::warn!("visible warning");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn filters_preserve_routing_scopes_and_runtime_destinations() {
        let info = ComponentLogRuntime::new(LogPolicy::new("info").unwrap()).unwrap();
        let warn = ComponentLogRuntime::new(LogPolicy::new("warn").unwrap()).unwrap();
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(info.layer()),
            || emit("one"),
        );
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(warn.layer()),
            || emit("two"),
        );
        info.flush().await.unwrap();
        warn.flush().await.unwrap();
        let one = ComponentLogKey::new("one", ComponentLogType::Source, "component");
        let two = ComponentLogKey::new("two", ComponentLogType::Source, "component");
        assert_eq!(info.registry().log_count_by_key(&one).await, 2);
        assert_eq!(info.registry().log_count_by_key(&two).await, 0);
        assert_eq!(warn.registry().log_count_by_key(&one).await, 0);
        let messages = warn.registry().get_history_by_key(&two).await;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].level, LogLevel::Warn);
        info.shutdown(Duration::from_secs(2)).await.unwrap();
        warn.shutdown(Duration::from_secs(2)).await.unwrap();
    }

    #[tokio::test]
    async fn trace_only_layers_do_not_enable_disabled_logs() {
        let capture = ComponentLogRuntime::new(LogPolicy::new("off").unwrap()).unwrap();
        let trace_layer = tracing_subscriber::fmt::layer()
            .with_writer(std::io::sink)
            .with_filter(filter_fn(|metadata| metadata.is_span()));
        tracing::subscriber::with_default(
            tracing_subscriber::registry()
                .with(capture.layer())
                .with(trace_layer),
            || emit("off"),
        );
        capture.flush().await.unwrap();
        assert_eq!(capture.registry().log_level(), Some(log::LevelFilter::Off));
        let key = ComponentLogKey::new("off", ComponentLogType::Source, "component");
        assert_eq!(capture.registry().log_count_by_key(&key).await, 0);
        capture.shutdown(Duration::from_secs(2)).await.unwrap();
    }

    #[test]
    fn invalid_configuration_does_not_start_capture() {
        assert!(LogPolicy::new("target=not-a-level").is_err());
        assert!(ComponentLogRuntime::with_capacity(LogPolicy::new("info").unwrap(), 0).is_err());
    }
}
