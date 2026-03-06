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

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use aws_sdk_sqs::{types::MessageAttributeValue, Client};
use aws_types::region::Region;
use handlebars::Handlebars;
use log::{debug, error, info, warn};
use serde_json::{Map, Value};
use std::collections::HashMap;
use uuid::Uuid;

use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::Reaction;

use crate::config::{QueryConfig, SqsReactionConfig, TemplateSpec};
use crate::SqsReactionBuilder;

pub struct SqsReaction {
    base: ReactionBase,
    config: SqsReactionConfig,
}

impl SqsReaction {
    pub fn builder(id: impl Into<String>) -> SqsReactionBuilder {
        SqsReactionBuilder::new(id)
    }

    pub fn new(id: impl Into<String>, queries: Vec<String>, config: SqsReactionConfig) -> Self {
        let id = id.into();
        let params = ReactionBaseParams::new(id, queries);
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: SqsReactionConfig,
        priority_queue_capacity: usize,
    ) -> Self {
        let id = id.into();
        let params = ReactionBaseParams::new(id, queries)
            .with_priority_queue_capacity(priority_queue_capacity);
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: SqsReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    pub(crate) fn register_json_helper(handlebars: &mut Handlebars<'_>) {
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
                        let json_str = serde_json::to_string(value.value())
                            .unwrap_or_else(|_| "null".to_string());
                        out.write(&json_str)?;
                    } else {
                        out.write("null")?;
                    }
                    Ok(())
                },
            ),
        );
    }

    fn resolve_query_config<'a>(
        query_id: &str,
        routes: &'a HashMap<String, QueryConfig>,
        default_template: &'a Option<QueryConfig>,
    ) -> Option<&'a QueryConfig> {
        routes
            .get(query_id)
            .or_else(|| {
                if query_id.contains('.') {
                    query_id
                        .rsplit('.')
                        .next()
                        .and_then(|name| routes.get(name))
                } else {
                    None
                }
            })
            .or(default_template.as_ref())
    }

    fn build_context(
        query_id: &str,
        operation: &str,
        result: &ResultDiff,
        timestamp: i64,
    ) -> Map<String, Value> {
        let mut context = Map::new();
        match result {
            ResultDiff::Add { data } => {
                context.insert("after".to_string(), data.clone());
            }
            ResultDiff::Update { before, after, .. } => {
                context.insert("before".to_string(), before.clone());
                context.insert("after".to_string(), after.clone());
            }
            ResultDiff::Delete { data } => {
                context.insert("before".to_string(), data.clone());
            }
            ResultDiff::Aggregation { .. } | ResultDiff::Noop => {}
        }
        context.insert("query_id".to_string(), Value::String(query_id.to_string()));
        context.insert(
            "query_name".to_string(),
            Value::String(query_id.to_string()),
        );
        context.insert(
            "operation".to_string(),
            Value::String(operation.to_string()),
        );
        context.insert("timestamp".to_string(), Value::Number(timestamp.into()));
        context
    }

    fn resolve_template_spec<'a>(
        config: &'a QueryConfig,
        result: &ResultDiff,
    ) -> Option<(&'a TemplateSpec, &'static str)> {
        match result {
            ResultDiff::Add { .. } => config.added.as_ref().map(|spec| (spec, "ADD")),
            ResultDiff::Update { .. } => config.updated.as_ref().map(|spec| (spec, "UPDATE")),
            ResultDiff::Delete { .. } => config.deleted.as_ref().map(|spec| (spec, "DELETE")),
            ResultDiff::Aggregation { .. } | ResultDiff::Noop => None,
        }
    }

    fn render_body(
        handlebars: &Handlebars<'_>,
        spec: &TemplateSpec,
        context: &Map<String, Value>,
        result: &ResultDiff,
    ) -> Result<String> {
        if !spec.body.is_empty() {
            return Ok(handlebars.render_template(&spec.body, context)?);
        }

        let fallback_json = match result {
            ResultDiff::Add { data } => data.clone(),
            ResultDiff::Delete { data } => data.clone(),
            ResultDiff::Update { .. } => serde_json::to_value(result)?,
            ResultDiff::Aggregation { .. } | ResultDiff::Noop => Value::Null,
        };

        Ok(serde_json::to_string(&fallback_json)?)
    }

    fn render_message_attributes(
        handlebars: &Handlebars<'_>,
        templates: &HashMap<String, String>,
        context: &Map<String, Value>,
    ) -> Result<HashMap<String, String>> {
        let mut attrs = HashMap::new();
        for (key, template) in templates {
            attrs.insert(key.clone(), handlebars.render_template(template, context)?);
        }
        Ok(attrs)
    }

    fn string_attribute(value: impl Into<String>) -> Result<MessageAttributeValue> {
        let attr = MessageAttributeValue::builder()
            .data_type("String")
            .string_value(value.into())
            .build();
        if attr.string_value().is_none() || attr.data_type().is_none() {
            return Err(anyhow!("failed to build SQS message attribute"));
        }
        Ok(attr)
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_sqs_message(
        client: &Client,
        queue_url: &str,
        body: String,
        operation: &str,
        query_id: &str,
        custom_attributes: HashMap<String, String>,
        fifo_queue: bool,
        message_group_id: Option<String>,
    ) -> Result<()> {
        let mut merged_attributes = HashMap::new();
        merged_attributes.insert("drasi-query-id".to_string(), query_id.to_string());
        merged_attributes.insert("drasi-operation".to_string(), operation.to_string());
        for (key, value) in custom_attributes {
            merged_attributes.insert(key, value);
        }

        let mut request = client
            .send_message()
            .queue_url(queue_url)
            .message_body(body);
        for (key, value) in merged_attributes {
            request = request.message_attributes(key, Self::string_attribute(value)?);
        }

        if fifo_queue {
            let group_id = message_group_id.unwrap_or_else(|| query_id.to_string());
            request = request
                .message_group_id(group_id)
                .message_deduplication_id(Uuid::new_v4().to_string());
        }

        let response = request.send().await?;
        debug!(
            "SQS send completed; message_id={}",
            response.message_id().unwrap_or_default()
        );
        Ok(())
    }
}

#[async_trait]
impl Reaction for SqsReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "sqs"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "queue_url".to_string(),
            serde_json::Value::String(self.config.queue_url.clone()),
        );
        props.insert(
            "fifo_queue".to_string(),
            serde_json::Value::Bool(self.config.fifo_queue),
        );
        if let Some(region) = &self.config.region {
            props.insert(
                "region".to_string(),
                serde_json::Value::String(region.clone()),
            );
        }
        if let Some(endpoint_url) = &self.config.endpoint_url {
            props.insert(
                "endpoint_url".to_string(),
                serde_json::Value::String(endpoint_url.clone()),
            );
        }
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
        log_component_start("SQS Reaction", &self.base.id);
        info!(
            "[{}] SQS reaction started for queue: {}",
            self.base.id, self.config.queue_url
        );

        if self.config.fifo_queue && !self.config.queue_url.ends_with(".fifo") {
            warn!(
                "[{}] fifo_queue=true but queue_url does not end with .fifo: {}",
                self.base.id, self.config.queue_url
            );
        }

        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting SQS reaction".to_string()),
            )
            .await?;

        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("SQS reaction started".to_string()),
            )
            .await?;

        let mut shutdown_rx = self.base.create_shutdown_channel().await;
        let status = self.base.status.clone();
        let reaction_id = self.base.id.clone();
        let priority_queue = self.base.priority_queue.clone();
        let queue_url = self.config.queue_url.clone();
        let region = self.config.region.clone();
        let endpoint_url = self.config.endpoint_url.clone();
        let fifo_queue = self.config.fifo_queue;
        let message_group_id_template = self.config.message_group_id_template.clone();
        let routes = self.config.routes.clone();
        let default_template = self.config.default_template.clone();

        let processing_task = tokio::spawn(async move {
            let mut loader = aws_config::from_env();
            if let Some(endpoint) = endpoint_url.clone() {
                loader = loader.endpoint_url(endpoint);
            }
            if let Some(region_name) = region {
                loader = loader.region(Region::new(region_name));
            }
            let shared_config = loader.load().await;
            let client = Client::new(&shared_config);

            let mut handlebars = Handlebars::new();
            Self::register_json_helper(&mut handlebars);

            loop {
                let query_result_arc = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_id}] Received shutdown signal, exiting processing loop");
                        break;
                    }
                    result = priority_queue.dequeue() => result,
                };
                let query_result = query_result_arc.as_ref();

                if !matches!(*status.read().await, ComponentStatus::Running) {
                    break;
                }
                if query_result.results.is_empty() {
                    continue;
                }

                let query_id = &query_result.query_id;
                let Some(query_config) =
                    Self::resolve_query_config(query_id, &routes, &default_template)
                else {
                    debug!(
                        "[{reaction_id}] No route/default template configured for query '{query_id}', skipping"
                    );
                    continue;
                };

                for result in &query_result.results {
                    let Some((spec, operation)) = Self::resolve_template_spec(query_config, result)
                    else {
                        continue;
                    };
                    let timestamp = chrono::Utc::now().timestamp_millis();
                    let context = Self::build_context(query_id, operation, result, timestamp);

                    let body = match Self::render_body(&handlebars, spec, &context, result) {
                        Ok(body) => body,
                        Err(e) => {
                            error!(
                                "[{reaction_id}] Failed rendering body for query '{query_id}' operation '{operation}': {e}"
                            );
                            continue;
                        }
                    };

                    let custom_attributes = match Self::render_message_attributes(
                        &handlebars,
                        &spec.message_attributes,
                        &context,
                    ) {
                        Ok(attrs) => attrs,
                        Err(e) => {
                            error!(
                                "[{reaction_id}] Failed rendering message attributes for query '{query_id}' operation '{operation}': {e}"
                            );
                            continue;
                        }
                    };

                    let message_group_id = if fifo_queue {
                        match &message_group_id_template {
                            Some(template) => {
                                match handlebars.render_template(template, &context) {
                                    Ok(rendered) => Some(rendered),
                                    Err(e) => {
                                        error!(
                                        "[{reaction_id}] Failed rendering message group id template '{template}': {e}"
                                    );
                                        Some(query_id.clone())
                                    }
                                }
                            }
                            None => Some(query_id.clone()),
                        }
                    } else {
                        None
                    };

                    if let Err(e) = Self::send_sqs_message(
                        &client,
                        &queue_url,
                        body,
                        operation,
                        query_id,
                        custom_attributes,
                        fifo_queue,
                        message_group_id,
                    )
                    .await
                    {
                        error!(
                            "[{reaction_id}] Failed to send SQS message for query '{query_id}' operation '{operation}': {e}"
                        );
                    }
                }
            }

            *status.write().await = ComponentStatus::Stopped;
            info!("[{reaction_id}] SQS reaction stopped");
        });
        self.base.set_processing_task(processing_task).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await?;

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("SQS reaction stopped successfully".to_string()),
            )
            .await?;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: drasi_lib::channels::QueryResult) -> Result<()> {
        self.base.enqueue_query_result(result).await
    }
}
