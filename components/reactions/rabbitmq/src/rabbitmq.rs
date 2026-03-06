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

use super::config::{ExchangeType, PublishSpec, QueryPublishConfig, RabbitMQReactionConfig};
use crate::RabbitMQReactionBuilder;
use anyhow::Result;
use async_trait::async_trait;
use handlebars::Handlebars;
use lapin::{
    options::{BasicPublishOptions, ExchangeDeclareOptions},
    types::{AMQPValue, FieldTable},
    BasicProperties, Channel, Connection, ConnectionProperties,
};
use log::{debug, error, info};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs;

use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::Reaction;

pub struct RabbitMQReaction {
    base: ReactionBase,
    config: RabbitMQReactionConfig,
}

impl RabbitMQReaction {
    /// Create a builder for RabbitMQReaction.
    pub fn builder(id: impl Into<String>) -> RabbitMQReactionBuilder {
        RabbitMQReactionBuilder::new(id)
    }

    /// Create a new RabbitMQ reaction.
    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: RabbitMQReactionConfig,
    ) -> anyhow::Result<Self> {
        Self::validate_config(&queries, &config)?;
        let params = ReactionBaseParams::new(id, queries);
        Ok(Self {
            base: ReactionBase::new(params),
            config,
        })
    }

    /// Create a new RabbitMQ reaction with custom priority queue capacity.
    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: RabbitMQReactionConfig,
        priority_queue_capacity: usize,
    ) -> anyhow::Result<Self> {
        Self::validate_config(&queries, &config)?;
        let params = ReactionBaseParams::new(id, queries)
            .with_priority_queue_capacity(priority_queue_capacity);
        Ok(Self {
            base: ReactionBase::new(params),
            config,
        })
    }

    /// Create from builder (internal method).
    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: RabbitMQReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> anyhow::Result<Self> {
        Self::validate_config(&queries, &config)?;
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        Ok(Self {
            base: ReactionBase::new(params),
            config,
        })
    }

    fn validate_template(template: &str) -> anyhow::Result<()> {
        if template.is_empty() {
            return Ok(());
        }
        handlebars::Template::compile(template)
            .map_err(|e| anyhow::anyhow!("Invalid template: {e}"))?;
        Ok(())
    }

    fn validate_publish_spec(spec: &PublishSpec) -> anyhow::Result<()> {
        Self::validate_template(&spec.routing_key)?;
        if let Some(body_template) = &spec.body_template {
            Self::validate_template(body_template)?;
        }
        for (key, value) in &spec.headers {
            if key.trim().is_empty() {
                return Err(anyhow::anyhow!("Header name must not be empty"));
            }
            Self::validate_template(value)?;
        }
        Ok(())
    }

    fn validate_query_config(config: &QueryPublishConfig) -> anyhow::Result<()> {
        if let Some(added) = &config.added {
            Self::validate_publish_spec(added)?;
        }
        if let Some(updated) = &config.updated {
            Self::validate_publish_spec(updated)?;
        }
        if let Some(deleted) = &config.deleted {
            Self::validate_publish_spec(deleted)?;
        }
        Ok(())
    }

    fn validate_config(queries: &[String], config: &RabbitMQReactionConfig) -> anyhow::Result<()> {
        config.validate()?;

        for (query_id, query_config) in &config.query_configs {
            Self::validate_query_config(query_config)
                .map_err(|e| anyhow::anyhow!("Invalid template in route '{query_id}': {e}"))?;
        }

        if !config.query_configs.is_empty() && !queries.is_empty() {
            for route_query in config.query_configs.keys() {
                let dotted_route = format!(".{route_query}");
                let matches = queries
                    .iter()
                    .any(|q| q == route_query || q.ends_with(&dotted_route));
                if !matches {
                    return Err(anyhow::anyhow!(
                        "Route '{route_query}' does not match any subscribed query. Subscribed queries: {queries:?}"
                    ));
                }
            }
        }

        Ok(())
    }

    fn build_tls_config(
        cert_path: Option<&str>,
        pfx_path: Option<&str>,
    ) -> anyhow::Result<lapin::tcp::OwnedTLSConfig> {
        let mut tls_config = lapin::tcp::OwnedTLSConfig::default();
        if let Some(cert_path) = cert_path {
            let cert_chain = fs::read_to_string(cert_path)?;
            tls_config.cert_chain = Some(cert_chain);
        }
        if let Some(pfx_path) = pfx_path {
            let identity_bytes = fs::read(pfx_path)?;
            tls_config.identity = Some(lapin::tcp::OwnedIdentity {
                der: identity_bytes,
                password: String::new(),
            });
        }
        Ok(tls_config)
    }

    fn register_helpers(handlebars: &mut Handlebars<'static>) {
        handlebars.register_helper(
            "json",
            Box::new(
                |h: &handlebars::Helper,
                 _: &Handlebars,
                 _: &handlebars::Context,
                 _: &mut handlebars::RenderContext,
                 out: &mut dyn handlebars::Output|
                 -> handlebars::HelperResult {
                    if let Some(value) = h.param(0) {
                        let json_str = serde_json::to_string(&value.value())
                            .unwrap_or_else(|_| "null".to_string());
                        out.write(&json_str)?;
                    }
                    Ok(())
                },
            ),
        );
    }

    /// Establish a connection and channel to RabbitMQ, declaring the exchange.
    #[allow(clippy::too_many_arguments)]
    async fn establish_connection(
        connection_string: &str,
        reaction_name: &str,
        tls_enabled: bool,
        tls_cert_path: Option<&str>,
        tls_pfx_path: Option<&str>,
        exchange_name: &str,
        exchange_type: &ExchangeType,
        exchange_durable: bool,
    ) -> Result<(Connection, Channel)> {
        let connection_props =
            ConnectionProperties::default().with_connection_name(reaction_name.to_string().into());

        let connection = if tls_enabled {
            let tls_config = Self::build_tls_config(tls_cert_path, tls_pfx_path)?;
            Connection::connect_with_config(connection_string, connection_props, tls_config).await?
        } else {
            Connection::connect(connection_string, connection_props).await?
        };

        let channel = connection.create_channel().await?;
        channel
            .exchange_declare(
                exchange_name,
                exchange_type.as_exchange_kind(),
                ExchangeDeclareOptions {
                    durable: exchange_durable,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await?;

        Ok((connection, channel))
    }

    #[allow(clippy::too_many_arguments)]
    async fn publish_result(
        channel: &Channel,
        handlebars: &Handlebars<'static>,
        exchange_name: &str,
        message_persistent: bool,
        publish_spec: &PublishSpec,
        operation: &str,
        before: Option<&Value>,
        after: Option<&Value>,
        raw_data: &Value,
        query_id: &str,
        timestamp: &chrono::DateTime<chrono::Utc>,
        metadata: &HashMap<String, Value>,
        reaction_name: &str,
    ) -> Result<()> {
        let mut context = Map::new();

        if let Some(before) = before {
            context.insert("before".to_string(), before.clone());
        }
        if let Some(after) = after {
            context.insert("after".to_string(), after.clone());
        }
        context.insert("data".to_string(), raw_data.clone());

        context.insert("query_id".to_string(), Value::String(query_id.to_string()));
        context.insert(
            "operation".to_string(),
            Value::String(operation.to_string()),
        );
        context.insert(
            "timestamp".to_string(),
            Value::String(timestamp.to_rfc3339()),
        );
        context.insert(
            "metadata".to_string(),
            Value::Object(metadata.clone().into_iter().collect()),
        );

        let routing_key = handlebars.render_template(&publish_spec.routing_key, &context)?;

        let body = if let Some(template) = &publish_spec.body_template {
            debug!(
                "[{reaction_name}] Rendering body template: {template} with context: {context:?}"
            );
            let rendered = handlebars.render_template(template, &context)?;
            debug!("[{reaction_name}] Rendered body: {rendered}");
            rendered
        } else {
            serde_json::to_string(raw_data)?
        };

        let mut headers = FieldTable::default();
        for (key, value_template) in &publish_spec.headers {
            let rendered_value = handlebars.render_template(value_template, &context)?;
            headers.insert(
                key.clone().into(),
                AMQPValue::LongString(rendered_value.into()),
            );
        }

        let mut properties =
            BasicProperties::default().with_content_type("application/json".into());
        if message_persistent {
            properties = properties.with_delivery_mode(2);
        }
        properties = properties.with_headers(headers);

        debug!(
            "[{reaction_name}] Publishing to exchange '{exchange_name}' with routing key '{routing_key}'"
        );

        channel
            .basic_publish(
                exchange_name,
                &routing_key,
                BasicPublishOptions::default(),
                body.as_bytes(),
                properties,
            )
            .await?
            .await?;

        Ok(())
    }
}

#[async_trait]
impl Reaction for RabbitMQReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "rabbitmq"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "exchange_name".to_string(),
            Value::String(self.config.exchange_name.clone()),
        );
        props.insert(
            "exchange_type".to_string(),
            Value::String(format!("{:?}", self.config.exchange_type)),
        );
        props.insert(
            "exchange_durable".to_string(),
            Value::Bool(self.config.exchange_durable),
        );
        props.insert(
            "message_persistent".to_string(),
            Value::Bool(self.config.message_persistent),
        );
        props.insert(
            "tls_enabled".to_string(),
            Value::Bool(self.config.tls_enabled),
        );
        props
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        log_component_start("RabbitMQ Reaction", &self.base.id);

        let reaction_name = self.base.id.clone();
        let exchange_name = self.config.exchange_name.clone();
        info!(
            "[{reaction_name}] RabbitMQ reaction starting - publishing to exchange: {exchange_name}"
        );

        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting RabbitMQ reaction".to_string()),
            )
            .await?;

        let mut shutdown_rx = self.base.create_shutdown_channel().await;
        let status = self.base.status.clone();
        let query_configs = self.config.query_configs.clone();
        let exchange_type = self.config.exchange_type.clone();
        let exchange_durable = self.config.exchange_durable;
        let message_persistent = self.config.message_persistent;
        let connection_string = self.config.connection_string.clone();
        let tls_enabled = self.config.tls_enabled;
        let tls_cert_path = self.config.tls_cert_path.clone();
        let tls_pfx_path = self.config.tls_pfx_path.clone();
        let priority_queue = self.base.priority_queue.clone();

        let processing_task_handle = tokio::spawn(async move {
            // Establish initial connection
            let (mut _connection, mut channel) = match Self::establish_connection(
                &connection_string,
                &reaction_name,
                tls_enabled,
                tls_cert_path.as_deref(),
                tls_pfx_path.as_deref(),
                &exchange_name,
                &exchange_type,
                exchange_durable,
            )
            .await
            {
                Ok(result) => result,
                Err(e) => {
                    error!("[{reaction_name}] Failed to connect to RabbitMQ: {e}");
                    *status.write().await = ComponentStatus::Error;
                    return;
                }
            };

            info!("[{reaction_name}] Connected to RabbitMQ successfully");
            *status.write().await = ComponentStatus::Running;

            let mut handlebars = Handlebars::new();
            Self::register_helpers(&mut handlebars);

            loop {
                let query_result_arc = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                        break;
                    }
                    result = priority_queue.dequeue() => result,
                };

                let query_result = query_result_arc.as_ref();

                if !matches!(*status.read().await, ComponentStatus::Running) {
                    break;
                }

                // Check connection health and reconnect if needed
                if !channel.status().connected() {
                    info!("[{reaction_name}] Channel disconnected, attempting to reconnect...");
                    let mut reconnected = false;
                    let mut shutdown_requested = false;
                    for attempt in 1u32..=5 {
                        let delay = std::time::Duration::from_secs(2u64.pow(attempt.min(4)));
                        // Respect shutdown signal during backoff sleep
                        tokio::select! {
                            biased;
                            _ = &mut shutdown_rx => {
                                debug!("[{reaction_name}] Shutdown signal received during reconnect backoff");
                                shutdown_requested = true;
                                break;
                            }
                            _ = tokio::time::sleep(delay) => {}
                        }
                        match Self::establish_connection(
                            &connection_string,
                            &reaction_name,
                            tls_enabled,
                            tls_cert_path.as_deref(),
                            tls_pfx_path.as_deref(),
                            &exchange_name,
                            &exchange_type,
                            exchange_durable,
                        )
                        .await
                        {
                            Ok((new_conn, new_chan)) => {
                                _connection = new_conn;
                                channel = new_chan;
                                info!(
                                    "[{reaction_name}] Reconnected to RabbitMQ on attempt {attempt}"
                                );
                                reconnected = true;
                                break;
                            }
                            Err(e) => {
                                error!("[{reaction_name}] Reconnect attempt {attempt} failed: {e}");
                            }
                        }
                    }
                    if !reconnected {
                        if !shutdown_requested {
                            error!(
                                "[{reaction_name}] Failed to reconnect after 5 attempts, shutting down"
                            );
                            *status.write().await = ComponentStatus::Error;
                        }
                        break;
                    }
                }

                if query_result.results.is_empty() {
                    debug!("[{reaction_name}] Received empty result set from query");
                    continue;
                }

                let query_name = &query_result.query_id;
                let query_config = query_configs.get(query_name).or_else(|| {
                    query_name
                        .split('.')
                        .next_back()
                        .and_then(|name| query_configs.get(name))
                });

                let default_config;
                let query_config = match query_config {
                    Some(config) => config,
                    None => {
                        debug!(
                            "[{reaction_name}] No configuration for query '{query_name}', using default"
                        );
                        default_config = QueryPublishConfig {
                            added: Some(PublishSpec {
                                routing_key: format!("changes.{query_name}"),
                                headers: HashMap::new(),
                                body_template: None,
                            }),
                            updated: Some(PublishSpec {
                                routing_key: format!("changes.{query_name}"),
                                headers: HashMap::new(),
                                body_template: None,
                            }),
                            deleted: Some(PublishSpec {
                                routing_key: format!("changes.{query_name}"),
                                headers: HashMap::new(),
                                body_template: None,
                            }),
                        };
                        &default_config
                    }
                };

                let result_count = query_result.results.len();
                debug!(
                    "[{reaction_name}] Processing {result_count} results from query '{query_name}'"
                );

                for result in &query_result.results {
                    match result {
                        ResultDiff::Add { data } => {
                            if let Some(spec) = query_config.added.as_ref() {
                                if let Err(e) = Self::publish_result(
                                    &channel,
                                    &handlebars,
                                    &exchange_name,
                                    message_persistent,
                                    spec,
                                    "ADD",
                                    None,
                                    Some(data),
                                    data,
                                    query_name,
                                    &query_result.timestamp,
                                    &query_result.metadata,
                                    &reaction_name,
                                )
                                .await
                                {
                                    error!("[{reaction_name}] Failed to publish result: {e}");
                                }
                            }
                        }
                        ResultDiff::Delete { data } => {
                            if let Some(spec) = query_config.deleted.as_ref() {
                                if let Err(e) = Self::publish_result(
                                    &channel,
                                    &handlebars,
                                    &exchange_name,
                                    message_persistent,
                                    spec,
                                    "DELETE",
                                    Some(data),
                                    None,
                                    data,
                                    query_name,
                                    &query_result.timestamp,
                                    &query_result.metadata,
                                    &reaction_name,
                                )
                                .await
                                {
                                    error!("[{reaction_name}] Failed to publish result: {e}");
                                }
                            }
                        }
                        ResultDiff::Update {
                            data,
                            before,
                            after,
                            ..
                        } => {
                            if let Some(spec) = query_config.updated.as_ref() {
                                if let Err(e) = Self::publish_result(
                                    &channel,
                                    &handlebars,
                                    &exchange_name,
                                    message_persistent,
                                    spec,
                                    "UPDATE",
                                    Some(before),
                                    Some(after),
                                    data,
                                    query_name,
                                    &query_result.timestamp,
                                    &query_result.metadata,
                                    &reaction_name,
                                )
                                .await
                                {
                                    error!("[{reaction_name}] Failed to publish result: {e}");
                                }
                            }
                        }
                        ResultDiff::Aggregation { .. } | ResultDiff::Noop => {
                            debug!("[{reaction_name}] Ignoring aggregation/noop result");
                        }
                    }
                }
            }

            info!("[{reaction_name}] RabbitMQ reaction stopped");
            *status.write().await = ComponentStatus::Stopped;
        });

        self.base.set_processing_task(processing_task_handle).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await?;

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("RabbitMQ reaction stopped successfully".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(
        &self,
        result: drasi_lib::channels::QueryResult,
    ) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }
}
