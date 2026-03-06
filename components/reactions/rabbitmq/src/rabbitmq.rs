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

use super::config::{PublishSpec, QueryPublishConfig, RabbitMQReactionConfig};
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
        key_path: Option<&str>,
    ) -> anyhow::Result<lapin::tcp::OwnedTLSConfig> {
        let mut tls_config = lapin::tcp::OwnedTLSConfig::default();
        if let Some(cert_path) = cert_path {
            let cert_chain = fs::read_to_string(cert_path)?;
            tls_config.cert_chain = Some(cert_chain);
        }
        if let Some(key_path) = key_path {
            let identity_bytes = fs::read(key_path)?;
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

    #[allow(clippy::too_many_arguments)]
    async fn publish_result(
        channel: &Channel,
        handlebars: &Handlebars<'static>,
        exchange_name: &str,
        message_persistent: bool,
        publish_spec: &PublishSpec,
        result_type: &str,
        data: &Value,
        query_id: &str,
        timestamp: &chrono::DateTime<chrono::Utc>,
        metadata: &HashMap<String, Value>,
        reaction_name: &str,
    ) -> Result<()> {
        let mut context = Map::new();

        match result_type {
            "ADD" => {
                context.insert("after".to_string(), data.clone());
            }
            "UPDATE" => {
                if let Some(obj) = data.as_object() {
                    if let Some(before) = obj.get("before") {
                        context.insert("before".to_string(), before.clone());
                    }
                    if let Some(after) = obj.get("after") {
                        context.insert("after".to_string(), after.clone());
                    }
                    if let Some(data_field) = obj.get("data") {
                        context.insert("data".to_string(), data_field.clone());
                    }
                } else {
                    context.insert("after".to_string(), data.clone());
                }
            }
            "DELETE" => {
                context.insert("before".to_string(), data.clone());
            }
            _ => {
                context.insert("data".to_string(), data.clone());
            }
        }

        context.insert("query_id".to_string(), Value::String(query_id.to_string()));
        context.insert(
            "operation".to_string(),
            Value::String(result_type.to_string()),
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
            serde_json::to_string(&data)?
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
            "[{reaction_name}] RabbitMQ reaction started - publishing to exchange: {exchange_name}"
        );

        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting RabbitMQ reaction".to_string()),
            )
            .await?;

        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("RabbitMQ reaction started".to_string()),
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
        let tls_key_path = self.config.tls_key_path.clone();
        let priority_queue = self.base.priority_queue.clone();

        let processing_task_handle = tokio::spawn(async move {
            let connection_props =
                ConnectionProperties::default().with_connection_name(reaction_name.clone().into());

            let connection_result = if tls_enabled {
                let tls_config = match RabbitMQReaction::build_tls_config(
                    tls_cert_path.as_deref(),
                    tls_key_path.as_deref(),
                ) {
                    Ok(config) => config,
                    Err(e) => {
                        error!("[{reaction_name}] Failed to load TLS config: {e}");
                        *status.write().await = ComponentStatus::Error;
                        return;
                    }
                };
                Connection::connect_with_config(&connection_string, connection_props, tls_config)
                    .await
            } else {
                Connection::connect(&connection_string, connection_props).await
            };

            let connection = match connection_result {
                Ok(conn) => conn,
                Err(e) => {
                    error!("[{reaction_name}] Failed to connect to RabbitMQ: {e}");
                    *status.write().await = ComponentStatus::Error;
                    return;
                }
            };

            let channel = match connection.create_channel().await {
                Ok(chan) => chan,
                Err(e) => {
                    error!("[{reaction_name}] Failed to create channel: {e}");
                    *status.write().await = ComponentStatus::Error;
                    return;
                }
            };

            if let Err(e) = channel
                .exchange_declare(
                    &exchange_name,
                    exchange_type.as_exchange_kind(),
                    ExchangeDeclareOptions {
                        durable: exchange_durable,
                        ..Default::default()
                    },
                    FieldTable::default(),
                )
                .await
            {
                error!("[{reaction_name}] Failed to declare exchange: {e}");
                *status.write().await = ComponentStatus::Error;
                return;
            }

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
                        ResultDiff::Update { .. } => {
                            if let Some(spec) = query_config.updated.as_ref() {
                                let data_to_process = serde_json::to_value(result)
                                    .expect("ResultDiff serialization should succeed");
                                if let Err(e) = Self::publish_result(
                                    &channel,
                                    &handlebars,
                                    &exchange_name,
                                    message_persistent,
                                    spec,
                                    "UPDATE",
                                    &data_to_process,
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
