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

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use async_trait::async_trait;
use futures_util::SinkExt;
use tokio::sync::{oneshot, Mutex};
use tracing::{error, info, warn};

use drasi_lib::{
    BootstrapProvider, ComponentStatus, DispatchMode, Source, SourceBase, SourceBaseParams,
    SourceRuntimeContext, SourceSchema, SourceSubscriptionSettings, SubscriptionResponse,
};

use crate::{
    config::{HeaderConfig, WebSocketSourceConfig},
    descriptor::WebSocketSourceConfigDto,
    mapping::{derive_schema, FrameMapper},
    transport::{self, SessionEnd, WebSocketConnection},
};

/// Generic outbound WebSocket source.
pub struct WebSocketSource {
    pub(crate) base: SourceBase,
    config: Arc<WebSocketSourceConfig>,
    lifecycle: Mutex<()>,
}

impl WebSocketSource {
    /// Creates a source from a complete configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration or source base is invalid.
    pub fn new(id: impl Into<String>, config: WebSocketSourceConfig) -> Result<Self> {
        Self::builder(id).with_config(config).build()
    }

    /// Creates a source builder.
    pub fn builder(id: impl Into<String>) -> WebSocketSourceBuilder {
        WebSocketSourceBuilder::new(id)
    }
}

/// Builder for [`WebSocketSource`].
pub struct WebSocketSourceBuilder {
    id: String,
    config: WebSocketSourceConfig,
    bootstrap_provider: Option<Box<dyn BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl WebSocketSourceBuilder {
    /// Creates a builder with configuration defaults.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: WebSocketSourceConfig::default(),
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    /// Replaces the complete source configuration.
    pub fn with_config(mut self, config: WebSocketSourceConfig) -> Self {
        self.config = config;
        self
    }

    /// Sets the WebSocket endpoint.
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.config.url = url.into();
        self
    }

    /// Adds one upgrade header.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.headers.push(HeaderConfig {
            name: name.into(),
            value: value.into(),
        });
        self
    }

    /// Adds one JSON message sent after each handshake.
    pub fn with_initial_message(mut self, message: impl Into<String>) -> Self {
        self.config.initial_messages.push(message.into());
        self
    }

    /// Adds one payload mapping.
    pub fn with_mapping(mut self, mapping: drasi_source_mapping::SourceMapping) -> Self {
        self.config.mappings.push(mapping);
        self
    }

    /// Sets whether Drasi starts the source automatically.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Sets an external bootstrap provider.
    pub fn with_bootstrap_provider(mut self, provider: impl BootstrapProvider + 'static) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    /// Validates the configuration and builds the source.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration or source base is invalid.
    pub fn build(self) -> Result<WebSocketSource> {
        self.config.validate()?;

        let params = SourceBaseParams {
            id: self.id,
            dispatch_mode: Some(DispatchMode::Channel),
            dispatch_buffer_capacity: Some(self.config.buffer_capacity),
            state_store: None,
            bootstrap_provider: self.bootstrap_provider,
            auto_start: self.auto_start,
        };

        Ok(WebSocketSource {
            base: SourceBase::new(params)?,
            config: Arc::new(self.config),
            lifecycle: Mutex::new(()),
        })
    }
}

impl Default for WebSocketSourceBuilder {
    fn default() -> Self {
        Self::new("websocket")
    }
}

#[async_trait]
impl Source for WebSocketSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "websocket"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        if let Some(serde_json::Value::Object(properties)) = self.base.raw_config() {
            return properties.clone().into_iter().collect();
        }

        self.base
            .properties_or_serialize(&WebSocketSourceConfigDto::from(self.config.as_ref()))
    }

    fn dispatch_mode(&self) -> DispatchMode {
        self.base.get_dispatch_mode()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    fn supports_replay(&self) -> bool {
        false
    }

    fn describe_schema(&self) -> Option<SourceSchema> {
        derive_schema(&self.config.mappings)
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        match self.base.get_status().await {
            ComponentStatus::Running | ComponentStatus::Starting => return Ok(()),
            ComponentStatus::Stopping => {
                anyhow::bail!("source '{}' is stopping", self.base.id)
            }
            _ => {}
        }

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Waiting for WebSocket subscribers".to_string()),
            )
            .await;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        self.base.set_shutdown_tx(shutdown_tx).await;

        let base = self.base.clone_shared();
        let config = self.config.clone();
        let task = tokio::spawn(run_worker(base, config, shutdown_rx));

        self.base.set_task_handle(task).await;
        info!(
            "[{}] WebSocket source waiting for subscribers",
            self.base.id
        );
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        if self.base.get_status().await == ComponentStatus::Stopped {
            self.base.clear_dispatchers().await;
            return Ok(());
        }
        self.base
            .set_status(
                ComponentStatus::Stopping,
                Some("Stopping WebSocket source".to_string()),
            )
            .await;
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "WebSocket")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn deprovision(&self) -> Result<()> {
        self.base.deprovision_common().await
    }

    async fn set_bootstrap_provider(&self, provider: Box<dyn BootstrapProvider + 'static>) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

async fn run_worker(
    base: SourceBase,
    config: Arc<WebSocketSourceConfig>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    tokio::select! {
        biased;
        _ = &mut shutdown_rx => return,
        _ = base.wait_for_subscribers() => {}
    }

    if base.get_status().await != ComponentStatus::Starting {
        return;
    }

    base.set_status(
        ComponentStatus::Starting,
        Some("Connecting WebSocket source".to_string()),
    )
    .await;

    let mut reconnect_backoff = ReconnectBackoff::new(
        config.reconnect.delay_ms,
        config.reconnect.effective_max_delay_ms(),
    );
    let mut socket = match connect_with_retry(
        &base,
        &config,
        &mut shutdown_rx,
        &mut reconnect_backoff,
        false,
        "WebSocket connection failed",
    )
    .await
    {
        Ok(Some(socket)) => socket,
        Ok(None) => return,
        Err(error) => {
            fail_source(&base, "WebSocket connection failed", &error).await;
            return;
        }
    };

    if base.get_status().await != ComponentStatus::Starting {
        let _ = socket.close(None).await;
        return;
    }

    base.set_status(
        ComponentStatus::Running,
        Some("WebSocket source connected".to_string()),
    )
    .await;
    info!("[{}] WebSocket source connected", base.id);

    let mapper = FrameMapper::new(&config);
    loop {
        match transport::run_session(&mut socket, &base, &mapper, &mut shutdown_rx).await {
            Ok(SessionEnd::Shutdown) => return,
            Ok(SessionEnd::Disconnected { clean }) => {
                if !config.reconnect.enabled {
                    base.clear_dispatchers().await;
                    let status = if clean {
                        ComponentStatus::Stopped
                    } else {
                        ComponentStatus::Error
                    };
                    base.set_status(status, Some("WebSocket connection closed".to_string()))
                        .await;
                    return;
                }
            }
            Err(error) => {
                fail_source(&base, "WebSocket message processing failed", &error).await;
                return;
            }
        }

        base.set_status(
            ComponentStatus::Starting,
            Some("Reconnecting WebSocket source".to_string()),
        )
        .await;

        let reconnected = match connect_with_retry(
            &base,
            &config,
            &mut shutdown_rx,
            &mut reconnect_backoff,
            true,
            "WebSocket reconnect failed",
        )
        .await
        {
            Ok(Some(socket)) => socket,
            Ok(None) => return,
            Err(error) => {
                fail_source(&base, "WebSocket reconnect failed", &error).await;
                return;
            }
        };
        socket = reconnected;
        if base.get_status().await != ComponentStatus::Starting {
            let _ = socket.close(None).await;
            return;
        }
        base.set_status(
            ComponentStatus::Running,
            Some("WebSocket source connected".to_string()),
        )
        .await;
        info!("[{}] WebSocket source reconnected", base.id);
    }
}

async fn connect_with_retry(
    base: &SourceBase,
    config: &WebSocketSourceConfig,
    shutdown_rx: &mut oneshot::Receiver<()>,
    backoff: &mut ReconnectBackoff,
    delay_before_first_attempt: bool,
    failure_summary: &str,
) -> Result<Option<WebSocketConnection>> {
    let mut delay_before_attempt = delay_before_first_attempt.then(|| backoff.next_delay());

    loop {
        if let Some(delay) = delay_before_attempt.take() {
            tokio::select! {
                biased;
                _ = &mut *shutdown_rx => return Ok(None),
                _ = tokio::time::sleep(delay) => {}
            }
        }

        let connection = tokio::select! {
            biased;
            _ = &mut *shutdown_rx => return Ok(None),
            connection = transport::connect(config) => connection,
        };

        match connection {
            Ok(socket) => {
                backoff.reset();
                return Ok(Some(socket));
            }
            Err(error)
                if config.reconnect.enabled
                    && transport::connect_error_disposition(&error)
                        == transport::ConnectErrorDisposition::Retry =>
            {
                let safe_error = transport::safe_error_description(&error);
                warn!("[{}] {failure_summary}: {safe_error}; retrying", base.id);
                let retry_summary = format!("{failure_summary}; retrying");
                base.set_status(
                    ComponentStatus::Starting,
                    Some(status_message(&retry_summary, &error)),
                )
                .await;
                delay_before_attempt = Some(backoff.next_delay());
            }
            Err(error) => return Err(error),
        }
    }
}

async fn fail_source(base: &SourceBase, summary: &str, error: &anyhow::Error) {
    let safe_error = transport::safe_error_description(error);
    error!("[{}] {summary}: {safe_error}", base.id);
    base.clear_dispatchers().await;
    base.set_status(ComponentStatus::Error, Some(status_message(summary, error)))
        .await;
}

fn status_message(summary: &str, error: &anyhow::Error) -> String {
    let safe_error = transport::safe_error_description(error);
    truncate_status_message(format!("{summary}: {safe_error}"))
}

fn truncate_status_message(message: String) -> String {
    const MAX_STATUS_CHARS: usize = 512;

    if message.chars().count() <= MAX_STATUS_CHARS {
        return message;
    }

    let mut truncated = message
        .chars()
        .take(MAX_STATUS_CHARS - 3)
        .collect::<String>();
    truncated.push_str("...");
    truncated
}

#[derive(Debug)]
struct ReconnectBackoff {
    initial_delay_ms: u64,
    max_delay_ms: u64,
    next_delay_ms: u64,
}

impl ReconnectBackoff {
    fn new(initial_delay_ms: u64, max_delay_ms: u64) -> Self {
        Self {
            initial_delay_ms,
            max_delay_ms,
            next_delay_ms: initial_delay_ms,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.next_delay_ms;
        self.next_delay_ms = self.next_delay_ms.saturating_mul(2).min(self.max_delay_ms);
        Duration::from_millis(delay)
    }

    fn reset(&mut self) {
        self.next_delay_ms = self.initial_delay_ms;
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };

    use drasi_lib::{
        bootstrap::{BootstrapContext, BootstrapRequest, BootstrapResult},
        channels::BootstrapEventSender,
    };
    use drasi_source_mapping::{ElementTemplate, ElementType, OperationType, SourceMapping};
    use tokio::time::timeout;

    use super::*;

    fn mapping() -> SourceMapping {
        SourceMapping {
            when: None,
            operation: Some(OperationType::Insert),
            operation_from: None,
            operation_map: None,
            element_type: ElementType::Node,
            effective_from: None,
            template: ElementTemplate {
                id: "{{payload.id}}".to_string(),
                labels: vec!["Item".to_string()],
                properties: None,
                from: None,
                to: None,
            },
        }
    }

    fn config() -> WebSocketSourceConfig {
        WebSocketSourceConfig {
            url: "wss://example.com/events?token=embedded-secret".to_string(),
            headers: vec![HeaderConfig {
                name: "Authorization".to_string(),
                value: "header-secret".to_string(),
            }],
            initial_messages: vec![r#"{"token":"message-secret"}"#.to_string()],
            mappings: vec![mapping()],
            ..Default::default()
        }
    }

    fn subscription_settings(query_id: &str, enable_bootstrap: bool) -> SourceSubscriptionSettings {
        SourceSubscriptionSettings {
            source_id: "source".to_string(),
            enable_bootstrap,
            query_id: query_id.to_string(),
            nodes: HashSet::from(["Item".to_string()]),
            relations: HashSet::new(),
            resume_from: None,
            request_position_handle: false,
        }
    }

    mod construction {
        use super::*;

        #[test]
        fn new_source_exposes_expected_identity_and_capabilities() {
            let source = WebSocketSource::new("source", config()).unwrap();

            assert_eq!(source.id(), "source");
            assert_eq!(source.type_name(), "websocket");
            assert_eq!(source.dispatch_mode(), DispatchMode::Channel);
            assert!(source.auto_start());
            assert!(!source.supports_replay());
        }
    }

    mod properties {
        use super::*;

        #[test]
        fn embedded_source_properties_round_trip_configuration() {
            let source = WebSocketSource::new("source", config()).unwrap();

            assert_eq!(
                serde_json::to_value(source.properties()).unwrap(),
                serde_json::json!({
                    "url": "wss://example.com/events?token=embedded-secret",
                    "allowInsecure": false,
                    "headers": [{
                        "name": "Authorization",
                        "value": "header-secret"
                    }],
                    "connectTimeoutMs": 10_000,
                    "initialMessages": [r#"{"token":"message-secret"}"#],
                    "reconnect": {
                        "enabled": true,
                        "delayMs": 1_000
                    },
                    "itemsPath": "$",
                    "mappings": [{
                        "operation": "insert",
                        "elementType": "node",
                        "template": {
                            "id": "{{payload.id}}",
                            "labels": ["Item"]
                        }
                    }],
                    "maxMessageSizeBytes": 1024 * 1024,
                    "bufferCapacity": 64
                })
            );
        }
    }

    mod builder {
        use super::*;

        #[test]
        fn default_trait_uses_builder_defaults() {
            let builder = WebSocketSourceBuilder::default();

            assert_eq!(builder.id, "websocket");
            assert_eq!(builder.config, WebSocketSourceConfig::default());
            assert!(builder.bootstrap_provider.is_none());
            assert!(builder.auto_start);
        }

        #[test]
        fn builder_rejects_insecure_url_without_opt_in() {
            let error = WebSocketSource::builder("invalid")
                .with_url("ws://example.com")
                .with_mapping(mapping())
                .build()
                .err()
                .expect("insecure URL should be rejected");
            assert_eq!(
                error.to_string(),
                "allowInsecure must be true for ws:// endpoints"
            );
        }

        #[test]
        fn builder_applies_headers_initial_messages_and_auto_start() {
            let source = WebSocketSource::builder("source")
                .with_url("wss://example.com")
                .with_header("X-Test", "value")
                .with_initial_message(r#"{"subscribe":true}"#)
                .with_mapping(mapping())
                .with_auto_start(false)
                .build()
                .unwrap();

            assert!(!source.auto_start());
            assert_eq!(
                source.config.headers,
                vec![HeaderConfig {
                    name: "X-Test".to_string(),
                    value: "value".to_string(),
                }]
            );
            assert_eq!(
                source.config.initial_messages,
                vec![r#"{"subscribe":true}"#.to_string()]
            );
        }
    }

    mod lifecycle {
        use super::*;

        #[tokio::test]
        async fn stop_cancels_worker_waiting_for_subscription() {
            let source = WebSocketSource::new("source", config()).unwrap();
            assert_eq!(source.status().await, ComponentStatus::Stopped);

            source.start().await.unwrap();
            assert_eq!(source.status().await, ComponentStatus::Starting);

            timeout(Duration::from_secs(1), source.stop())
                .await
                .expect("stop should cancel subscriber waiting")
                .unwrap();
            assert_eq!(source.status().await, ComponentStatus::Stopped);
        }

        #[tokio::test]
        async fn subscribe_returns_streaming_response_without_bootstrap_or_position() {
            let source = WebSocketSource::new("source", config()).unwrap();
            let response = source
                .subscribe(subscription_settings("query", false))
                .await
                .unwrap();

            assert_eq!(response.query_id, "query");
            assert_eq!(response.source_id, "source");
            assert!(response.bootstrap_receiver.is_none());
            assert!(response.position_handle.is_none());
        }

        #[tokio::test]
        async fn stop_clears_subscription_registered_while_stopped() {
            let source = WebSocketSource::new("source", config()).unwrap();
            let response = source
                .subscribe(subscription_settings("query", false))
                .await
                .unwrap();
            let mut receiver = response.receiver;

            source.stop().await.unwrap();

            assert!(timeout(Duration::from_secs(1), receiver.recv())
                .await
                .expect("stopped source should close its subscriber channel")
                .is_err());
        }
    }

    mod bootstrap {
        use super::*;

        struct RecordingBootstrapProvider {
            called: Arc<AtomicBool>,
        }

        #[async_trait]
        impl BootstrapProvider for RecordingBootstrapProvider {
            async fn bootstrap(
                &self,
                _request: BootstrapRequest,
                _context: &BootstrapContext,
                _event_tx: BootstrapEventSender,
                _settings: Option<&SourceSubscriptionSettings>,
            ) -> Result<BootstrapResult> {
                self.called.store(true, Ordering::SeqCst);
                Ok(BootstrapResult {
                    event_count: 0,
                    source_position: None,
                })
            }
        }

        #[tokio::test]
        async fn builder_attaches_bootstrap_provider() {
            let called = Arc::new(AtomicBool::new(false));
            let source = WebSocketSource::builder("source")
                .with_config(config())
                .with_bootstrap_provider(RecordingBootstrapProvider {
                    called: called.clone(),
                })
                .build()
                .unwrap();

            let mut response = source
                .subscribe(subscription_settings("builder-bootstrap-query", true))
                .await
                .unwrap();
            let result = response
                .bootstrap_result_receiver
                .take()
                .unwrap()
                .await
                .unwrap()
                .unwrap();

            assert!(called.load(Ordering::SeqCst));
            assert!(result.source_position.is_none());
        }

        #[tokio::test]
        async fn set_bootstrap_provider_delegates_without_a_source_position() {
            let called = Arc::new(AtomicBool::new(false));
            let source = WebSocketSource::builder("source")
                .with_config(config())
                .build()
                .unwrap();
            source
                .set_bootstrap_provider(Box::new(RecordingBootstrapProvider {
                    called: called.clone(),
                }))
                .await;

            let mut response = source
                .subscribe(subscription_settings("bootstrap-query", true))
                .await
                .unwrap();
            assert!(response.bootstrap_receiver.is_some());
            let result = response
                .bootstrap_result_receiver
                .take()
                .unwrap()
                .await
                .unwrap()
                .unwrap();

            assert!(called.load(Ordering::SeqCst));
            assert_eq!(result.event_count, 0);
            assert!(result.source_position.is_none());
        }
    }

    mod backoff {
        use super::*;

        #[test]
        fn backoff_grows_safely_caps_and_resets() {
            let mut backoff = ReconnectBackoff::new(1_000, 4_000);

            assert_eq!(backoff.next_delay(), Duration::from_millis(1_000));
            assert_eq!(backoff.next_delay(), Duration::from_millis(2_000));
            assert_eq!(backoff.next_delay(), Duration::from_millis(4_000));
            assert_eq!(backoff.next_delay(), Duration::from_millis(4_000));

            backoff.reset();
            assert_eq!(backoff.next_delay(), Duration::from_millis(1_000));
        }

        #[test]
        fn backoff_growth_is_overflow_safe() {
            let mut backoff = ReconnectBackoff::new(u64::MAX - 1, u64::MAX);
            assert_eq!(backoff.next_delay(), Duration::from_millis(u64::MAX - 1));
            assert_eq!(backoff.next_delay(), Duration::from_millis(u64::MAX));
            assert_eq!(backoff.next_delay(), Duration::from_millis(u64::MAX));
        }
    }

    mod diagnostics {
        use super::*;

        #[test]
        fn status_messages_omit_unclassified_error_details() {
            let error = anyhow::anyhow!("url-secret header-secret message-secret");
            let message = status_message("WebSocket connection failed", &error);

            assert_eq!(
                message,
                "WebSocket connection failed: WebSocket operation failed"
            );
            assert!(!message.contains("url-secret"));
            assert!(!message.contains("header-secret"));
            assert!(!message.contains("message-secret"));
        }

        #[test]
        fn status_messages_truncate_long_summaries() {
            let message = status_message(&"x".repeat(600), &anyhow::anyhow!("details"));

            assert_eq!(message.chars().count(), 512);
            assert!(message.ends_with("..."));
        }
    }
}
