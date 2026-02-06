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
//! This module provides the storage and broadcast infrastructure for component logs.
//! Logs are captured by the tracing layer (`ComponentLogLayer`) and routed to
//! per-component streams that can be subscribed to by clients.
//!
//! # Architecture
//!
//! - `ComponentLogRegistry`: Central registry that manages log channels and history
//! - `LogMessage`: Structured log message with timestamp, level, and metadata
//! - `LogLevel`: Log severity levels (Trace, Debug, Info, Warn, Error)
//!
//! # Usage
//!
//! Log capture is automatic when code runs within a tracing span that has
//! `component_id` and `component_type` attributes:
//!
//! ```ignore
//! use tracing::Instrument;
//!
//! let span = tracing::info_span!(
//!     "source",
//!     component_id = %source_id,
//!     component_type = "source"
//! );
//!
//! async {
//!     // Both of these are captured:
//!     tracing::info!("Starting source");
//!     log::info!("Also captured via tracing-log bridge");
//! }.instrument(span).await;
//! ```
//!
//! Subscribers can stream logs from a component:
//!
//! ```ignore
//! let (history, mut receiver) = core.subscribe_source_logs("my-source").await?;
//! while let Ok(log) = receiver.recv().await {
//!     println!("[{}] {}: {}", log.level, log.component_id, log.message);
//! }
//! ```

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};

use crate::channels::ComponentType;

/// Default maximum number of log messages to retain per component.
pub const DEFAULT_MAX_LOGS_PER_COMPONENT: usize = 100;

/// Composite key for identifying a component's log channel.
///
/// This ensures logs from different DrasiLib instances with the same component ID
/// are kept separate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComponentLogKey {
    /// The DrasiLib instance ID (or empty string for legacy/default behavior)
    pub instance_id: String,
    /// The type of component (Source, Query, Reaction)
    pub component_type: ComponentType,
    /// The component's ID within its DrasiLib instance
    pub component_id: String,
}

impl ComponentLogKey {
    /// Create a new composite key.
    pub fn new(
        instance_id: impl Into<String>,
        component_type: ComponentType,
        component_id: impl Into<String>,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            component_type,
            component_id: component_id.into(),
        }
    }

    /// Create a key from string representation (for backwards compatibility).
    /// Format: "instance_id:component_type:component_id" or just "component_id" for legacy.
    pub fn from_str_key(key: &str) -> Option<Self> {
        let parts: Vec<&str> = key.split(':').collect();
        match parts.len() {
            1 => None, // Legacy single-part key, can't reconstruct
            3 => {
                let component_type = match parts[1].to_lowercase().as_str() {
                    "source" => ComponentType::Source,
                    "query" => ComponentType::Query,
                    "reaction" => ComponentType::Reaction,
                    _ => return None,
                };
                Some(Self {
                    instance_id: parts[0].to_string(),
                    component_type,
                    component_id: parts[2].to_string(),
                })
            }
            _ => None,
        }
    }

    /// Convert to string key for HashMap storage.
    pub fn to_string_key(&self) -> String {
        let type_str = match self.component_type {
            ComponentType::Source => "source",
            ComponentType::Query => "query",
            ComponentType::Reaction => "reaction",
        };
        format!("{}:{}:{}", self.instance_id, type_str, self.component_id)
    }
}

impl std::fmt::Display for ComponentLogKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_string_key())
    }
}

/// Default broadcast channel capacity for live log streaming.
pub const DEFAULT_LOG_CHANNEL_CAPACITY: usize = 256;

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
    /// ID of the DrasiLib instance that owns the component
    pub instance_id: String,
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
        Self::with_instance(level, message, "", component_id, component_type)
    }

    /// Create a new log message with instance ID.
    pub fn with_instance(
        level: LogLevel,
        message: impl Into<String>,
        instance_id: impl Into<String>,
        component_id: impl Into<String>,
        component_type: ComponentType,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            level,
            message: message.into(),
            instance_id: instance_id.into(),
            component_id: component_id.into(),
            component_type,
        }
    }

    /// Get the composite key for this log message.
    pub fn key(&self) -> ComponentLogKey {
        ComponentLogKey::new(
            self.instance_id.clone(),
            self.component_type.clone(),
            self.component_id.clone(),
        )
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
    /// Uses composite key (instance_id:component_type:component_id) for storage.
    pub async fn log(&self, message: LogMessage) {
        let key = message.key().to_string_key();
        let mut channels = self.channels.write().await;
        let channel = channels
            .entry(key)
            .or_insert_with(|| ComponentLogChannel::new(self.max_history, self.channel_capacity));
        channel.log(message);
    }

    /// Get the log history for a component using composite key.
    ///
    /// Returns an empty vector if the component has no logs.
    pub async fn get_history_by_key(&self, key: &ComponentLogKey) -> Vec<LogMessage> {
        let channels = self.channels.read().await;
        channels
            .get(&key.to_string_key())
            .map(|c| c.get_history())
            .unwrap_or_default()
    }

    /// Get the log history for a component (legacy API, uses empty instance_id).
    ///
    /// Returns an empty vector if the component has no logs.
    #[deprecated(note = "Use get_history_by_key with ComponentLogKey for instance isolation")]
    pub async fn get_history(&self, component_id: &str) -> Vec<LogMessage> {
        let channels = self.channels.read().await;
        channels
            .get(component_id)
            .map(|c| c.get_history())
            .unwrap_or_default()
    }

    /// Subscribe to live logs for a component using composite key.
    ///
    /// Returns the current history and a broadcast receiver for new logs.
    /// Creates the component's channel if it doesn't exist.
    pub async fn subscribe_by_key(
        &self,
        key: &ComponentLogKey,
    ) -> (Vec<LogMessage>, broadcast::Receiver<LogMessage>) {
        let mut channels = self.channels.write().await;
        let channel = channels
            .entry(key.to_string_key())
            .or_insert_with(|| ComponentLogChannel::new(self.max_history, self.channel_capacity));

        let history = channel.get_history();
        let receiver = channel.subscribe();
        (history, receiver)
    }

    /// Subscribe to live logs for a component (legacy API).
    ///
    /// Returns the current history and a broadcast receiver for new logs.
    /// Creates the component's channel if it doesn't exist.
    #[deprecated(note = "Use subscribe_by_key with ComponentLogKey for instance isolation")]
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

    /// Remove a component's log channel using composite key.
    ///
    /// Called when a component is deleted to clean up resources.
    pub async fn remove_component_by_key(&self, key: &ComponentLogKey) {
        self.channels.write().await.remove(&key.to_string_key());
    }

    /// Remove a component's log channel (legacy API).
    ///
    /// Called when a component is deleted to clean up resources.
    #[deprecated(note = "Use remove_component_by_key with ComponentLogKey for instance isolation")]
    pub async fn remove_component(&self, component_id: &str) {
        self.channels.write().await.remove(component_id);
    }

    /// Get the number of log messages stored for a component using composite key.
    pub async fn log_count_by_key(&self, key: &ComponentLogKey) -> usize {
        self.channels
            .read()
            .await
            .get(&key.to_string_key())
            .map(|c| c.history.len())
            .unwrap_or(0)
    }

    /// Get the number of log messages stored for a component (legacy API).
    #[deprecated(note = "Use log_count_by_key with ComponentLogKey for instance isolation")]
    pub async fn log_count(&self, component_id: &str) -> usize {
        self.channels
            .read()
            .await
            .get(component_id)
            .map(|c| c.history.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{sleep, Duration};

    fn make_key(instance: &str, component_type: ComponentType, component: &str) -> ComponentLogKey {
        ComponentLogKey::new(instance, component_type, component)
    }

    #[tokio::test]
    async fn test_log_and_get_history() {
        let registry = ComponentLogRegistry::new();

        let msg1 = LogMessage::with_instance(
            LogLevel::Info,
            "First message",
            "instance1",
            "source1",
            ComponentType::Source,
        );
        let msg2 = LogMessage::with_instance(
            LogLevel::Error,
            "Second message",
            "instance1",
            "source1",
            ComponentType::Source,
        );

        registry.log(msg1).await;
        registry.log(msg2).await;

        let key = make_key("instance1", ComponentType::Source, "source1");
        let history = registry.get_history_by_key(&key).await;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].message, "First message");
        assert_eq!(history[1].message, "Second message");
        assert_eq!(history[1].level, LogLevel::Error);
    }

    #[tokio::test]
    async fn test_max_history_limit() {
        let registry = ComponentLogRegistry::with_capacity(3, 10);

        for i in 0..5 {
            let msg = LogMessage::with_instance(
                LogLevel::Info,
                format!("Message {i}"),
                "instance1",
                "source1",
                ComponentType::Source,
            );
            registry.log(msg).await;
        }

        let key = make_key("instance1", ComponentType::Source, "source1");
        let history = registry.get_history_by_key(&key).await;
        assert_eq!(history.len(), 3);
        // Should have messages 2, 3, 4 (oldest removed)
        assert_eq!(history[0].message, "Message 2");
        assert_eq!(history[2].message, "Message 4");
    }

    #[tokio::test]
    async fn test_subscribe_gets_history_and_live() {
        let registry = Arc::new(ComponentLogRegistry::new());

        // Log some history first
        let msg1 = LogMessage::with_instance(
            LogLevel::Info,
            "History 1",
            "instance1",
            "source1",
            ComponentType::Source,
        );
        registry.log(msg1).await;

        // Subscribe
        let key = make_key("instance1", ComponentType::Source, "source1");
        let (history, mut receiver) = registry.subscribe_by_key(&key).await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].message, "History 1");

        // Log a new message after subscribing
        let registry_clone = registry.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(10)).await;
            let msg2 = LogMessage::with_instance(
                LogLevel::Info,
                "Live message",
                "instance1",
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
    async fn test_remove_component() {
        let registry = ComponentLogRegistry::new();

        let msg = LogMessage::with_instance(
            LogLevel::Info,
            "Test",
            "instance1",
            "source1",
            ComponentType::Source,
        );
        registry.log(msg).await;

        let key = make_key("instance1", ComponentType::Source, "source1");
        assert_eq!(registry.log_count_by_key(&key).await, 1);

        registry.remove_component_by_key(&key).await;

        assert_eq!(registry.log_count_by_key(&key).await, 0);
    }

    #[tokio::test]
    async fn test_multiple_components() {
        let registry = ComponentLogRegistry::new();

        let msg1 = LogMessage::with_instance(
            LogLevel::Info,
            "Source log",
            "instance1",
            "source1",
            ComponentType::Source,
        );
        let msg2 = LogMessage::with_instance(
            LogLevel::Info,
            "Query log",
            "instance1",
            "query1",
            ComponentType::Query,
        );

        registry.log(msg1).await;
        registry.log(msg2).await;

        let source_key = make_key("instance1", ComponentType::Source, "source1");
        let query_key = make_key("instance1", ComponentType::Query, "query1");

        let source_history = registry.get_history_by_key(&source_key).await;
        let query_history = registry.get_history_by_key(&query_key).await;

        assert_eq!(source_history.len(), 1);
        assert_eq!(query_history.len(), 1);
        assert_eq!(source_history[0].component_type, ComponentType::Source);
        assert_eq!(query_history[0].component_type, ComponentType::Query);
    }

    #[tokio::test]
    async fn test_instance_isolation() {
        // Test that different instances with the same component ID are isolated
        let registry = ComponentLogRegistry::new();

        // Same component ID, different instances
        let msg1 = LogMessage::with_instance(
            LogLevel::Info,
            "Instance 1 log",
            "instance1",
            "my-source",
            ComponentType::Source,
        );
        let msg2 = LogMessage::with_instance(
            LogLevel::Info,
            "Instance 2 log",
            "instance2",
            "my-source",
            ComponentType::Source,
        );

        registry.log(msg1).await;
        registry.log(msg2).await;

        let key1 = make_key("instance1", ComponentType::Source, "my-source");
        let key2 = make_key("instance2", ComponentType::Source, "my-source");

        let history1 = registry.get_history_by_key(&key1).await;
        let history2 = registry.get_history_by_key(&key2).await;

        // Each instance should only see its own logs
        assert_eq!(history1.len(), 1);
        assert_eq!(history2.len(), 1);
        assert_eq!(history1[0].message, "Instance 1 log");
        assert_eq!(history2[0].message, "Instance 2 log");
    }

    #[test]
    fn test_component_log_key() {
        let key = ComponentLogKey::new("my-instance", ComponentType::Source, "my-source");
        assert_eq!(key.to_string_key(), "my-instance:source:my-source");
        assert_eq!(key.instance_id, "my-instance");
        assert_eq!(key.component_type, ComponentType::Source);
        assert_eq!(key.component_id, "my-source");
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
}
