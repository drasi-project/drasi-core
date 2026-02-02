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

//! Component logging infrastructure for live log streaming.
//!
//! This module provides a custom logging system that allows components to emit
//! structured log messages that can be streamed in real-time to subscribers.
//! It is separate from (but complements) the standard Rust `log` crate.
//!
//! # Architecture
//!
//! - `ComponentLogRegistry`: Central registry that manages log channels and history
//! - `ComponentLogger`: Logger instance given to each component for emitting logs
//! - `LogMessage`: Structured log message with timestamp, level, and metadata
//!
//! # Usage
//!
//! Components receive a `ComponentLogger` via their runtime context and use it
//! to emit logs:
//!
//! ```ignore
//! // In a source implementation
//! self.logger().info("Starting data ingestion");
//! self.logger().error("Connection failed: timeout");
//! ```
//!
//! Subscribers can stream logs from a component:
//!
//! ```ignore
//! let mut logs = core.subscribe_source_logs("my-source").await?;
//! while let Some(log) = logs.next().await {
//!     println!("[{}] {}: {}", log.level, log.component_id, log.message);
//! }
//! ```

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};

use crate::channels::ComponentType;

/// Default maximum number of log messages to retain per component.
pub const DEFAULT_MAX_LOGS_PER_COMPONENT: usize = 100;

/// Default broadcast channel capacity for live log streaming.
pub const DEFAULT_LOG_CHANNEL_CAPACITY: usize = 256;

// ============================================================================
// Global Registry for Log Routing
// ============================================================================

/// Global registry for component logs.
/// Used by ComponentAwareLogger to route log::info!() etc to component streams.
static GLOBAL_LOG_REGISTRY: OnceLock<Arc<ComponentLogRegistry>> = OnceLock::new();

/// Get or initialize the global log registry.
pub fn global_log_registry() -> Arc<ComponentLogRegistry> {
    GLOBAL_LOG_REGISTRY
        .get_or_init(|| Arc::new(ComponentLogRegistry::new()))
        .clone()
}

/// Initialize the global component-aware logger.
/// Call once at startup. Returns error if logger already set.
pub fn init_global_component_logger() -> Result<(), log::SetLoggerError> {
    let registry = global_log_registry();
    ComponentAwareLogger::new(registry).init()
}

// ============================================================================
// Task-Local Component Context
// ============================================================================

/// Context identifying the current component for log routing.
#[derive(Clone, Debug)]
pub struct ComponentContext {
    pub component_id: String,
    pub component_type: ComponentType,
}

impl ComponentContext {
    pub fn new(component_id: impl Into<String>, component_type: ComponentType) -> Self {
        Self {
            component_id: component_id.into(),
            component_type,
        }
    }
}

tokio::task_local! {
    /// Task-local storage for current component context.
    static COMPONENT_CONTEXT: ComponentContext;
}

/// Run an async block with the given component context.
/// All log::info!(), log::error!() etc calls within will be routed to the component's log stream.
pub async fn with_component_context<F, R>(context: ComponentContext, f: F) -> R
where
    F: std::future::Future<Output = R>,
{
    COMPONENT_CONTEXT.scope(context, f).await
}

/// Get the current component context if one is set.
pub fn current_component_context() -> Option<ComponentContext> {
    COMPONENT_CONTEXT.try_with(|ctx| ctx.clone()).ok()
}

// ============================================================================
// Component-Aware Logger
// ============================================================================

/// Logger that routes log crate calls to component log streams.
pub struct ComponentAwareLogger {
    registry: Arc<ComponentLogRegistry>,
    max_level: log::LevelFilter,
}

impl ComponentAwareLogger {
    pub fn new(registry: Arc<ComponentLogRegistry>) -> Self {
        Self {
            registry,
            max_level: log::LevelFilter::Debug,
        }
    }

    pub fn init(self) -> Result<(), log::SetLoggerError> {
        let max_level = self.max_level;
        log::set_boxed_logger(Box::new(self))?;
        log::set_max_level(max_level);
        Ok(())
    }

    fn convert_level(level: log::Level) -> LogLevel {
        match level {
            log::Level::Error => LogLevel::Error,
            log::Level::Warn => LogLevel::Warn,
            log::Level::Info => LogLevel::Info,
            log::Level::Debug => LogLevel::Debug,
            log::Level::Trace => LogLevel::Trace,
        }
    }
}

impl log::Log for ComponentAwareLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.max_level
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        // Route to component log stream if in component context
        if let Some(ctx) = current_component_context() {
            let log_message = LogMessage::new(
                Self::convert_level(record.level()),
                record.args().to_string(),
                ctx.component_id,
                ctx.component_type,
            );

            let registry = self.registry.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    registry.log(log_message).await;
                });
            }
        }

        // Also print to stderr
        eprintln!(
            "[{}] {} - {}",
            record.level(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {}
}

/// Log severity level.
///
/// Follows standard log level conventions, from most verbose (Trace) to
/// least verbose (Error).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LogLevel {
    /// Very detailed tracing information
    Trace,
    /// Debugging information
    Debug,
    /// General informational messages
    Info,
    /// Warning messages
    Warn,
    /// Error messages
    Error,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Trace => write!(f, "TRACE"),
            LogLevel::Debug => write!(f, "DEBUG"),
            LogLevel::Info => write!(f, "INFO"),
            LogLevel::Warn => write!(f, "WARN"),
            LogLevel::Error => write!(f, "ERROR"),
        }
    }
}

/// A structured log message from a component.
///
/// Contains the log content along with metadata about when and where
/// the log was generated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogMessage {
    /// Timestamp when the log was emitted
    pub timestamp: DateTime<Utc>,
    /// Severity level of the log
    pub level: LogLevel,
    /// The log message content
    pub message: String,
    /// ID of the component that emitted the log
    pub component_id: String,
    /// Type of the component (Source, Query, Reaction)
    pub component_type: ComponentType,
}

impl LogMessage {
    /// Create a new log message with the current timestamp.
    pub fn new(
        level: LogLevel,
        message: impl Into<String>,
        component_id: impl Into<String>,
        component_type: ComponentType,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            level,
            message: message.into(),
            component_id: component_id.into(),
            component_type,
        }
    }
}

/// Per-component log storage and broadcast channel.
struct ComponentLogChannel {
    /// Recent log history
    history: VecDeque<LogMessage>,
    /// Maximum history size
    max_history: usize,
    /// Broadcast sender for live streaming
    sender: broadcast::Sender<LogMessage>,
}

impl ComponentLogChannel {
    fn new(max_history: usize, channel_capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(channel_capacity);
        Self {
            history: VecDeque::with_capacity(max_history),
            max_history,
            sender,
        }
    }

    fn log(&mut self, message: LogMessage) {
        // Add to history
        if self.history.len() >= self.max_history {
            self.history.pop_front();
        }
        self.history.push_back(message.clone());

        // Broadcast to live subscribers (ignore if no subscribers)
        let _ = self.sender.send(message);
    }

    fn get_history(&self) -> Vec<LogMessage> {
        self.history.iter().cloned().collect()
    }

    fn subscribe(&self) -> broadcast::Receiver<LogMessage> {
        self.sender.subscribe()
    }
}

/// Central registry for component log channels.
///
/// Manages per-component log storage and broadcast channels for live streaming.
/// This is typically owned by `DrasiLib` and shared across all managers.
pub struct ComponentLogRegistry {
    /// Log channels per component ID
    channels: RwLock<HashMap<String, ComponentLogChannel>>,
    /// Maximum log history per component
    max_history: usize,
    /// Broadcast channel capacity
    channel_capacity: usize,
}

impl std::fmt::Debug for ComponentLogRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentLogRegistry")
            .field("max_history", &self.max_history)
            .field("channel_capacity", &self.channel_capacity)
            .finish()
    }
}

impl Default for ComponentLogRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ComponentLogRegistry {
    /// Create a new registry with default settings.
    pub fn new() -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
            max_history: DEFAULT_MAX_LOGS_PER_COMPONENT,
            channel_capacity: DEFAULT_LOG_CHANNEL_CAPACITY,
        }
    }

    /// Create a new registry with custom settings.
    pub fn with_capacity(max_history: usize, channel_capacity: usize) -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
            max_history,
            channel_capacity,
        }
    }

    /// Log a message for a component.
    ///
    /// Creates the component's channel if it doesn't exist.
    pub async fn log(&self, message: LogMessage) {
        let mut channels = self.channels.write().await;
        let channel = channels
            .entry(message.component_id.clone())
            .or_insert_with(|| ComponentLogChannel::new(self.max_history, self.channel_capacity));
        channel.log(message);
    }

    /// Get the log history for a component.
    ///
    /// Returns an empty vector if the component has no logs.
    pub async fn get_history(&self, component_id: &str) -> Vec<LogMessage> {
        let channels = self.channels.read().await;
        channels
            .get(component_id)
            .map(|c| c.get_history())
            .unwrap_or_default()
    }

    /// Subscribe to live logs for a component.
    ///
    /// Returns the current history and a broadcast receiver for new logs.
    /// Creates the component's channel if it doesn't exist.
    pub async fn subscribe(
        &self,
        component_id: &str,
    ) -> (Vec<LogMessage>, broadcast::Receiver<LogMessage>) {
        let mut channels = self.channels.write().await;
        let channel = channels
            .entry(component_id.to_string())
            .or_insert_with(|| ComponentLogChannel::new(self.max_history, self.channel_capacity));

        let history = channel.get_history();
        let receiver = channel.subscribe();
        (history, receiver)
    }

    /// Remove a component's log channel.
    ///
    /// Called when a component is deleted to clean up resources.
    pub async fn remove_component(&self, component_id: &str) {
        self.channels.write().await.remove(component_id);
    }

    /// Get the number of log messages stored for a component.
    pub async fn log_count(&self, component_id: &str) -> usize {
        self.channels
            .read()
            .await
            .get(component_id)
            .map(|c| c.history.len())
            .unwrap_or(0)
    }

    /// Create a logger for a component.
    ///
    /// The returned ComponentLogger can be passed to components via their
    /// runtime context to emit structured log messages.
    pub fn create_logger(
        self: &Arc<Self>,
        component_id: impl Into<String>,
        component_type: ComponentType,
    ) -> ComponentLogger {
        ComponentLogger::new(component_id, component_type, self.clone())
    }
}

/// Logger instance for a specific component.
///
/// Components receive this via their runtime context and use it to emit
/// structured log messages that can be streamed by subscribers.
///
/// # Example
///
/// ```ignore
/// // In a source implementation
/// async fn start(&self) -> Result<()> {
///     self.logger().info("Starting source").await;
///     
///     match self.connect().await {
///         Ok(_) => self.logger().info("Connected successfully").await,
///         Err(e) => self.logger().error(format!("Connection failed: {}", e)).await,
///     }
///     
///     Ok(())
/// }
/// ```
#[derive(Clone)]
pub struct ComponentLogger {
    component_id: String,
    component_type: ComponentType,
    registry: Arc<ComponentLogRegistry>,
}

impl std::fmt::Debug for ComponentLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentLogger")
            .field("component_id", &self.component_id)
            .field("component_type", &self.component_type)
            .finish()
    }
}

impl ComponentLogger {
    /// Create a new logger for a component.
    pub fn new(
        component_id: impl Into<String>,
        component_type: ComponentType,
        registry: Arc<ComponentLogRegistry>,
    ) -> Self {
        Self {
            component_id: component_id.into(),
            component_type,
            registry,
        }
    }

    /// Log a message at the specified level.
    pub async fn log(&self, level: LogLevel, message: impl Into<String>) {
        let log_message = LogMessage::new(
            level,
            message,
            self.component_id.clone(),
            self.component_type.clone(),
        );
        self.registry.log(log_message).await;
    }

    /// Log a trace message.
    pub async fn trace(&self, message: impl Into<String>) {
        self.log(LogLevel::Trace, message).await;
    }

    /// Log a debug message.
    pub async fn debug(&self, message: impl Into<String>) {
        self.log(LogLevel::Debug, message).await;
    }

    /// Log an info message.
    pub async fn info(&self, message: impl Into<String>) {
        self.log(LogLevel::Info, message).await;
    }

    /// Log a warning message.
    pub async fn warn(&self, message: impl Into<String>) {
        self.log(LogLevel::Warn, message).await;
    }

    /// Log an error message.
    pub async fn error(&self, message: impl Into<String>) {
        self.log(LogLevel::Error, message).await;
    }

    /// Get the component ID this logger is for.
    pub fn component_id(&self) -> &str {
        &self.component_id
    }

    /// Get the component type this logger is for.
    pub fn component_type(&self) -> &ComponentType {
        &self.component_type
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn test_log_and_get_history() {
        let registry = ComponentLogRegistry::new();

        let msg1 = LogMessage::new(
            LogLevel::Info,
            "First message",
            "source1",
            ComponentType::Source,
        );
        let msg2 = LogMessage::new(
            LogLevel::Error,
            "Second message",
            "source1",
            ComponentType::Source,
        );

        registry.log(msg1).await;
        registry.log(msg2).await;

        let history = registry.get_history("source1").await;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].message, "First message");
        assert_eq!(history[1].message, "Second message");
        assert_eq!(history[1].level, LogLevel::Error);
    }

    #[tokio::test]
    async fn test_max_history_limit() {
        let registry = ComponentLogRegistry::with_capacity(3, 10);

        for i in 0..5 {
            let msg = LogMessage::new(
                LogLevel::Info,
                format!("Message {i}"),
                "source1",
                ComponentType::Source,
            );
            registry.log(msg).await;
        }

        let history = registry.get_history("source1").await;
        assert_eq!(history.len(), 3);
        // Should have messages 2, 3, 4 (oldest removed)
        assert_eq!(history[0].message, "Message 2");
        assert_eq!(history[2].message, "Message 4");
    }

    #[tokio::test]
    async fn test_subscribe_gets_history_and_live() {
        let registry = Arc::new(ComponentLogRegistry::new());

        // Log some history first
        let msg1 = LogMessage::new(
            LogLevel::Info,
            "History 1",
            "source1",
            ComponentType::Source,
        );
        registry.log(msg1).await;

        // Subscribe
        let (history, mut receiver) = registry.subscribe("source1").await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].message, "History 1");

        // Log a new message after subscribing
        let registry_clone = registry.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(10)).await;
            let msg2 = LogMessage::new(
                LogLevel::Info,
                "Live message",
                "source1",
                ComponentType::Source,
            );
            registry_clone.log(msg2).await;
        });

        // Should receive the live message
        let live_msg = receiver.recv().await.unwrap();
        assert_eq!(live_msg.message, "Live message");
    }

    #[tokio::test]
    async fn test_component_logger() {
        let registry = Arc::new(ComponentLogRegistry::new());
        let logger = ComponentLogger::new("source1", ComponentType::Source, registry.clone());

        logger.info("Info message").await;
        logger.error("Error message").await;
        logger.debug("Debug message").await;

        let history = registry.get_history("source1").await;
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].level, LogLevel::Info);
        assert_eq!(history[1].level, LogLevel::Error);
        assert_eq!(history[2].level, LogLevel::Debug);
    }

    #[tokio::test]
    async fn test_remove_component() {
        let registry = ComponentLogRegistry::new();

        let msg = LogMessage::new(LogLevel::Info, "Test", "source1", ComponentType::Source);
        registry.log(msg).await;

        assert_eq!(registry.log_count("source1").await, 1);

        registry.remove_component("source1").await;

        assert_eq!(registry.log_count("source1").await, 0);
    }

    #[tokio::test]
    async fn test_multiple_components() {
        let registry = ComponentLogRegistry::new();

        let msg1 = LogMessage::new(
            LogLevel::Info,
            "Source log",
            "source1",
            ComponentType::Source,
        );
        let msg2 = LogMessage::new(LogLevel::Info, "Query log", "query1", ComponentType::Query);

        registry.log(msg1).await;
        registry.log(msg2).await;

        let source_history = registry.get_history("source1").await;
        let query_history = registry.get_history("query1").await;

        assert_eq!(source_history.len(), 1);
        assert_eq!(query_history.len(), 1);
        assert_eq!(source_history[0].component_type, ComponentType::Source);
        assert_eq!(query_history[0].component_type, ComponentType::Query);
    }

    #[test]
    fn test_log_level_ordering() {
        assert!(LogLevel::Trace < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);
    }

    #[test]
    fn test_log_level_display() {
        assert_eq!(format!("{}", LogLevel::Trace), "TRACE");
        assert_eq!(format!("{}", LogLevel::Debug), "DEBUG");
        assert_eq!(format!("{}", LogLevel::Info), "INFO");
        assert_eq!(format!("{}", LogLevel::Warn), "WARN");
        assert_eq!(format!("{}", LogLevel::Error), "ERROR");
    }

    #[tokio::test]
    async fn test_component_logger_logs_to_registry() {
        let registry = Arc::new(ComponentLogRegistry::new());
        let logger = registry.create_logger("test-source", ComponentType::Source);

        // Log messages via ComponentLogger
        logger.trace("trace message").await;
        logger.debug("debug message").await;
        logger.info("info message").await;
        logger.warn("warn message").await;
        logger.error("error message").await;

        // Verify messages are in registry
        let history = registry.get_history("test-source").await;
        assert_eq!(history.len(), 5);
        assert_eq!(history[0].level, LogLevel::Trace);
        assert_eq!(history[0].message, "trace message");
        assert_eq!(history[1].level, LogLevel::Debug);
        assert_eq!(history[2].level, LogLevel::Info);
        assert_eq!(history[3].level, LogLevel::Warn);
        assert_eq!(history[4].level, LogLevel::Error);
    }

    #[tokio::test]
    async fn test_component_logger_streams_to_subscribers() {
        let registry = Arc::new(ComponentLogRegistry::new());
        let logger = registry.create_logger("test-source", ComponentType::Source);

        // Subscribe before logging
        let (history, mut receiver) = registry.subscribe("test-source").await;
        assert!(history.is_empty());

        // Log a message
        logger.info("streaming test").await;

        // Should receive the message via broadcast
        let received = tokio::time::timeout(Duration::from_millis(100), receiver.recv())
            .await
            .expect("Timeout waiting for log")
            .expect("Channel error");

        assert_eq!(received.level, LogLevel::Info);
        assert_eq!(received.message, "streaming test");
        assert_eq!(received.component_id, "test-source");
        assert_eq!(received.component_type, ComponentType::Source);
    }

    #[tokio::test]
    async fn test_multiple_subscribers_receive_logs() {
        let registry = Arc::new(ComponentLogRegistry::new());
        let logger = registry.create_logger("test-source", ComponentType::Source);

        // Create two subscribers
        let (_history1, mut receiver1) = registry.subscribe("test-source").await;
        let (_history2, mut receiver2) = registry.subscribe("test-source").await;

        // Log a message
        logger.info("broadcast test").await;

        // Both should receive the message
        let received1 = tokio::time::timeout(Duration::from_millis(100), receiver1.recv())
            .await
            .expect("Timeout on receiver1")
            .expect("Channel error on receiver1");

        let received2 = tokio::time::timeout(Duration::from_millis(100), receiver2.recv())
            .await
            .expect("Timeout on receiver2")
            .expect("Channel error on receiver2");

        assert_eq!(received1.message, "broadcast test");
        assert_eq!(received2.message, "broadcast test");
    }

    #[tokio::test]
    async fn test_late_subscriber_gets_history() {
        let registry = Arc::new(ComponentLogRegistry::new());
        let logger = registry.create_logger("test-source", ComponentType::Source);

        // Log some messages before subscribing
        logger.info("message 1").await;
        logger.info("message 2").await;
        logger.info("message 3").await;

        // Subscribe after messages are logged
        let (history, _receiver) = registry.subscribe("test-source").await;

        // History should contain all 3 messages
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].message, "message 1");
        assert_eq!(history[1].message, "message 2");
        assert_eq!(history[2].message, "message 3");
    }

    #[tokio::test]
    async fn test_logger_clone_shares_registry() {
        let registry = Arc::new(ComponentLogRegistry::new());
        let logger1 = registry.create_logger("test-source", ComponentType::Source);
        let logger2 = logger1.clone();

        // Both loggers should write to the same component
        logger1.info("from logger1").await;
        logger2.info("from logger2").await;

        let history = registry.get_history("test-source").await;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].message, "from logger1");
        assert_eq!(history[1].message, "from logger2");
    }
}
