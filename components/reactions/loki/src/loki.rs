// Copyright 2026 The Drasi Authors.
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

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use handlebars::Handlebars;
use log::{debug, error, info, warn};
use reqwest::Client;
use serde::Serialize;
use serde_json::{Map, Value};

use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::Reaction;

use super::config::{LokiReactionConfig, QueryConfig};
use super::LokiReactionBuilder;

#[derive(Debug, Clone, Serialize)]
struct LokiStreamPayload {
    stream: HashMap<String, String>,
    values: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize)]
struct LokiPushPayload {
    streams: Vec<LokiStreamPayload>,
}

#[derive(Debug, Clone)]
struct BufferedStream {
    labels: HashMap<String, String>,
    values: Vec<(String, String)>,
}

pub struct LokiReaction {
    base: ReactionBase,
    config: LokiReactionConfig,
}

impl LokiReaction {
    pub fn builder(id: impl Into<String>) -> LokiReactionBuilder {
        LokiReactionBuilder::new(id)
    }

    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: LokiReactionConfig,
    ) -> Result<Self> {
        Self::from_builder(id.into(), queries, config, None, true)
    }

    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: LokiReactionConfig,
        priority_queue_capacity: usize,
    ) -> Result<Self> {
        Self::from_builder(
            id.into(),
            queries,
            config,
            Some(priority_queue_capacity),
            true,
        )
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: LokiReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Result<Self> {
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

    fn validate_template(template: &str) -> Result<()> {
        if template.is_empty() {
            return Ok(());
        }
        handlebars::Template::compile(template)
            .map_err(|e| anyhow::anyhow!("invalid template: {e}"))?;
        Ok(())
    }

    fn validate_query_config(config: &QueryConfig) -> Result<()> {
        if let Some(spec) = &config.added {
            Self::validate_template(&spec.template)?;
        }
        if let Some(spec) = &config.updated {
            Self::validate_template(&spec.template)?;
        }
        if let Some(spec) = &config.deleted {
            Self::validate_template(&spec.template)?;
        }
        Ok(())
    }

    fn validate_config(queries: &[String], config: &LokiReactionConfig) -> Result<()> {
        for (query_id, route_config) in &config.routes {
            Self::validate_query_config(route_config)
                .map_err(|e| anyhow::anyhow!("invalid template in route '{query_id}': {e}"))?;
        }

        if let Some(default_template) = &config.default_template {
            Self::validate_query_config(default_template)
                .map_err(|e| anyhow::anyhow!("invalid default template: {e}"))?;
        }

        for (label_name, label_value_template) in &config.labels {
            Self::validate_template(label_value_template).map_err(|e| {
                anyhow::anyhow!("invalid label template for label '{label_name}': {e}")
            })?;
        }

        if !config.routes.is_empty() && !queries.is_empty() {
            for route_query in config.routes.keys() {
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

    fn register_json_helper(handlebars: &mut Handlebars<'static>) {
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
                        match serde_json::to_string(&value.value()) {
                            Ok(json_str) => out.write(&json_str)?,
                            Err(_) => out.write("null")?,
                        }
                    }
                    Ok(())
                },
            ),
        );
    }

    fn operation_name(result: &ResultDiff) -> Option<&'static str> {
        match result {
            ResultDiff::Add { .. } => Some("ADD"),
            ResultDiff::Update { .. } => Some("UPDATE"),
            ResultDiff::Delete { .. } => Some("DELETE"),
            ResultDiff::Aggregation { .. } | ResultDiff::Noop => None,
        }
    }

    fn build_context(
        query_name: &str,
        operation: &str,
        result: &ResultDiff,
        timestamp: chrono::DateTime<Utc>,
    ) -> Map<String, Value> {
        let mut context = Map::new();
        context.insert(
            "query_name".to_string(),
            Value::String(query_name.to_string()),
        );
        context.insert(
            "operation".to_string(),
            Value::String(operation.to_string()),
        );
        context.insert(
            "timestamp".to_string(),
            Value::String(timestamp.to_rfc3339()),
        );

        match result {
            ResultDiff::Add { data } => {
                context.insert("after".to_string(), data.clone());
            }
            ResultDiff::Delete { data } => {
                context.insert("before".to_string(), data.clone());
            }
            ResultDiff::Update {
                before,
                after,
                data,
                ..
            } => {
                context.insert("before".to_string(), before.clone());
                context.insert("after".to_string(), after.clone());
                context.insert("data".to_string(), data.clone());
            }
            ResultDiff::Aggregation { after, before } => {
                context.insert("after".to_string(), after.clone());
                if let Some(before) = before {
                    context.insert("before".to_string(), before.clone());
                }
            }
            ResultDiff::Noop => {}
        }
        context
    }

    fn render_labels(
        handlebars: &Handlebars<'static>,
        label_templates: &HashMap<String, String>,
        context: &Map<String, Value>,
        reaction_name: &str,
    ) -> HashMap<String, String> {
        let mut labels = HashMap::new();
        for (key, value_template) in label_templates {
            match handlebars.render_template(value_template, context) {
                Ok(rendered) => {
                    labels.insert(key.clone(), rendered);
                }
                Err(e) => {
                    warn!("[{reaction_name}] failed to render label template for '{key}': {e}");
                    labels.insert(key.clone(), value_template.clone());
                }
            }
        }
        labels
    }

    fn query_template<'a>(
        query_name: &str,
        routes: &'a HashMap<String, QueryConfig>,
        default_template: &'a Option<QueryConfig>,
    ) -> Option<&'a QueryConfig> {
        routes
            .get(query_name)
            .or_else(|| {
                query_name
                    .split('.')
                    .next_back()
                    .and_then(|name| routes.get(name))
            })
            .or(default_template.as_ref())
    }

    fn render_log_line(
        handlebars: &Handlebars<'static>,
        query_template: Option<&QueryConfig>,
        result: &ResultDiff,
        context: &Map<String, Value>,
    ) -> Result<Option<String>> {
        let template = query_template.and_then(|query_template| match result {
            ResultDiff::Add { .. } => query_template
                .added
                .as_ref()
                .map(|spec| spec.template.as_str()),
            ResultDiff::Update { .. } => query_template
                .updated
                .as_ref()
                .map(|spec| spec.template.as_str()),
            ResultDiff::Delete { .. } => query_template
                .deleted
                .as_ref()
                .map(|spec| spec.template.as_str()),
            ResultDiff::Aggregation { .. } | ResultDiff::Noop => None,
        });

        if let Some(template) = template {
            if template.is_empty() {
                return Ok(Self::default_log_line(result));
            }
            let rendered = handlebars.render_template(template, context)?;
            return Ok(Some(rendered));
        }

        Ok(Self::default_log_line(result))
    }

    fn default_log_line(result: &ResultDiff) -> Option<String> {
        let value = match result {
            ResultDiff::Add { data } => data,
            ResultDiff::Delete { data } => data,
            ResultDiff::Update { data, .. } => data,
            ResultDiff::Aggregation { .. } | ResultDiff::Noop => return None,
        };

        serde_json::to_string(value).ok()
    }

    fn labels_key(labels: &HashMap<String, String>) -> Result<String> {
        let mut sorted = BTreeMap::new();
        for (k, v) in labels {
            sorted.insert(k.clone(), v.clone());
        }
        Ok(serde_json::to_string(&sorted)?)
    }

    fn push_endpoint(endpoint: &str) -> String {
        if endpoint.ends_with("/loki/api/v1/push") {
            endpoint.to_string()
        } else {
            format!("{}/loki/api/v1/push", endpoint.trim_end_matches('/'))
        }
    }

    async fn push_to_loki(
        client: &Client,
        endpoint: &str,
        token: &Option<String>,
        basic_auth: &Option<super::config::BasicAuth>,
        tenant_id: &Option<String>,
        payload: &LokiPushPayload,
    ) -> Result<()> {
        let push_endpoint = Self::push_endpoint(endpoint);
        let mut request = client.post(push_endpoint).json(payload);

        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(basic_auth) = basic_auth {
            request = request.basic_auth(&basic_auth.username, Some(&basic_auth.password));
        }
        if let Some(tenant_id) = tenant_id {
            request = request.header("X-Scope-OrgID", tenant_id);
        }

        let response = request.send().await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = match response.text().await {
                Ok(text) => text,
                Err(_) => "unable to read response body".to_string(),
            };
            return Err(anyhow::anyhow!(
                "loki push failed: status={status}, body={body}"
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl Reaction for LokiReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "loki"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "endpoint".to_string(),
            serde_json::Value::String(self.config.endpoint.clone()),
        );
        props.insert(
            "timeout_ms".to_string(),
            serde_json::Value::Number(self.config.timeout_ms.into()),
        );
        props.insert(
            "labels".to_string(),
            serde_json::to_value(&self.config.labels).unwrap_or(serde_json::Value::Null),
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
        log_component_start("Loki Reaction", &self.base.id);
        info!(
            "[{}] Loki reaction started - endpoint: {}",
            self.base.id, self.config.endpoint
        );

        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting Loki reaction".to_string()),
            )
            .await?;

        self.base.subscribe_to_queries().await?;

        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("Loki reaction started".to_string()),
            )
            .await?;

        let mut shutdown_rx = self.base.create_shutdown_channel().await;
        let reaction_name = self.base.id.clone();
        let status = self.base.status.clone();
        let priority_queue = self.base.priority_queue.clone();
        let endpoint = self.config.endpoint.clone();
        let token = self.config.token.clone();
        let basic_auth = self.config.basic_auth.clone();
        let tenant_id = self.config.tenant_id.clone();
        let timeout_ms = self.config.timeout_ms;
        let labels = self.config.labels.clone();
        let routes = self.config.routes.clone();
        let default_template = self.config.default_template.clone();

        let processing_task = tokio::spawn(async move {
            let client = match Client::builder()
                .timeout(std::time::Duration::from_millis(timeout_ms))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    error!("[{reaction_name}] failed to create HTTP client: {e}");
                    return;
                }
            };

            let mut handlebars = Handlebars::new();
            Self::register_json_helper(&mut handlebars);

            loop {
                let query_result_arc = tokio::select! {
                    biased;

                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_name}] received shutdown signal");
                        break;
                    }

                    result = priority_queue.dequeue() => result,
                };

                if !matches!(*status.read().await, ComponentStatus::Running) {
                    break;
                }

                let query_result = query_result_arc.as_ref();
                if query_result.results.is_empty() {
                    continue;
                }

                let query_name = &query_result.query_id;
                let query_template = Self::query_template(query_name, &routes, &default_template);
                let timestamp_ns = query_result
                    .timestamp
                    .timestamp_nanos_opt()
                    .unwrap_or_else(|| Utc::now().timestamp_nanos_opt().unwrap_or(0))
                    .to_string();

                let mut streams: HashMap<String, BufferedStream> = HashMap::new();

                for result in &query_result.results {
                    let operation = match Self::operation_name(result) {
                        Some(op) => op,
                        None => continue,
                    };

                    let context =
                        Self::build_context(query_name, operation, result, query_result.timestamp);
                    let mut rendered_labels =
                        Self::render_labels(&handlebars, &labels, &context, &reaction_name);
                    rendered_labels.insert("query_id".to_string(), query_name.clone());
                    rendered_labels.insert("reaction_id".to_string(), reaction_name.clone());
                    rendered_labels.insert("operation".to_string(), operation.to_string());

                    let log_line = match Self::render_log_line(
                        &handlebars,
                        query_template,
                        result,
                        &context,
                    ) {
                        Ok(Some(line)) => line,
                        Ok(None) => continue,
                        Err(e) => {
                            warn!(
                                    "[{reaction_name}] failed to render log line for query '{query_name}': {e}"
                                );
                            continue;
                        }
                    };

                    let key = match Self::labels_key(&rendered_labels) {
                        Ok(k) => k,
                        Err(e) => {
                            warn!(
                                "[{reaction_name}] failed to compute stream key for query '{query_name}': {e}"
                            );
                            continue;
                        }
                    };

                    let stream = streams.entry(key).or_insert_with(|| BufferedStream {
                        labels: rendered_labels.clone(),
                        values: Vec::new(),
                    });
                    stream.values.push((timestamp_ns.clone(), log_line));
                }

                if streams.is_empty() {
                    continue;
                }

                let payload = LokiPushPayload {
                    streams: streams
                        .into_values()
                        .map(|stream| LokiStreamPayload {
                            stream: stream.labels,
                            values: stream.values,
                        })
                        .collect(),
                };

                if let Err(e) = Self::push_to_loki(
                    &client,
                    &endpoint,
                    &token,
                    &basic_auth,
                    &tenant_id,
                    &payload,
                )
                .await
                {
                    warn!("[{reaction_name}] failed to push query '{query_name}' results to Loki: {e}");
                }
            }

            info!("[{reaction_name}] Loki reaction stopped");
            *status.write().await = ComponentStatus::Stopped;
        });

        self.base.set_processing_task(processing_task).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await?;
        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("Loki reaction stopped".to_string()),
            )
            .await?;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::channels::ResultDiff;

    #[test]
    fn test_dynamic_label_rendering() {
        let mut handlebars = Handlebars::new();
        LokiReaction::register_json_helper(&mut handlebars);

        let mut context = Map::new();
        context.insert(
            "after".to_string(),
            serde_json::json!({"type": "thermostat"}),
        );
        context.insert("operation".to_string(), Value::String("ADD".to_string()));

        let mut labels = HashMap::new();
        labels.insert("job".to_string(), "drasi".to_string());
        labels.insert("sensor_type".to_string(), "{{after.type}}".to_string());

        let rendered = LokiReaction::render_labels(&handlebars, &labels, &context, "test");
        assert_eq!(rendered.get("job"), Some(&"drasi".to_string()));
        assert_eq!(rendered.get("sensor_type"), Some(&"thermostat".to_string()));
    }

    #[test]
    fn test_render_log_line_with_template() {
        let mut handlebars = Handlebars::new();
        LokiReaction::register_json_helper(&mut handlebars);

        let result = ResultDiff::Add {
            data: serde_json::json!({"id": "sensor-1", "temperature": 42.1}),
        };
        let context = LokiReaction::build_context("q1", "ADD", &result, Utc::now());
        let query_config = QueryConfig {
            added: Some(super::super::TemplateSpec::new(
                "[{{operation}}] {{after.id}}={{after.temperature}}",
            )),
            ..Default::default()
        };

        let rendered =
            LokiReaction::render_log_line(&handlebars, Some(&query_config), &result, &context)
                .expect("render should succeed");
        assert_eq!(rendered, Some("[ADD] sensor-1=42.1".to_string()));
    }

    #[test]
    fn test_labels_key_is_stable() {
        let labels1 = HashMap::from([
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ]);
        let labels2 = HashMap::from([
            ("b".to_string(), "2".to_string()),
            ("a".to_string(), "1".to_string()),
        ]);

        let key1 = LokiReaction::labels_key(&labels1).expect("key1");
        let key2 = LokiReaction::labels_key(&labels2).expect("key2");
        assert_eq!(key1, key2);
    }
}
