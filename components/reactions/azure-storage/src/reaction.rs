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

//! Azure Storage reaction implementation.

use anyhow::{Context, Result};
use async_trait::async_trait;
use handlebars::Handlebars;
use log::{debug, error, info, warn};
use serde_json::{Map, Value};
use std::collections::HashMap;

use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::TemplateRouting;
use drasi_lib::reactions::common::{OperationType, TemplateSpec};
use drasi_lib::Reaction;
use drasi_lib::{ReactionBase, ReactionBaseParams};

use crate::blob::BlobService;
use crate::config::{AzureStorageReactionConfig, StorageTarget};
use crate::queue::QueueService;
use crate::table::TableService;
use crate::AzureStorageReactionBuilder;

enum ServiceClient {
    Blob(BlobService),
    Queue(QueueService),
    Table(TableService),
}

pub struct AzureStorageReaction {
    base: ReactionBase,
    config: AzureStorageReactionConfig,
}

impl AzureStorageReaction {
    pub fn builder(id: impl Into<String>) -> AzureStorageReactionBuilder {
        AzureStorageReactionBuilder::new(id)
    }

    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: AzureStorageReactionConfig,
    ) -> Result<Self> {
        Self::create_internal(id.into(), queries, config, None, true)
    }

    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: AzureStorageReactionConfig,
        priority_queue_capacity: usize,
    ) -> Result<Self> {
        Self::create_internal(
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
        config: AzureStorageReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Result<Self> {
        Self::create_internal(id, queries, config, priority_queue_capacity, auto_start)
    }

    fn create_internal(
        id: String,
        queries: Vec<String>,
        config: AzureStorageReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Result<Self> {
        let config = config.normalized();
        config.validate()?;

        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }

        Ok(Self {
            base: ReactionBase::new(params),
            config,
        })
    }

    fn resolve_template_spec<'a>(
        config: &'a AzureStorageReactionConfig,
        query_name: &str,
        operation: OperationType,
    ) -> Option<&'a TemplateSpec> {
        if let Some(spec) = config.get_template_spec(query_name, operation) {
            return Some(spec);
        }

        query_name
            .rsplit('.')
            .next()
            .and_then(|short| config.get_template_spec(short, operation))
    }

    fn build_context(
        operation_name: &str,
        query_name: &str,
        data: &Value,
    ) -> serde_json::Map<String, Value> {
        let mut context = serde_json::Map::new();

        match operation_name {
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
                    if let Some(raw) = obj.get("data") {
                        context.insert("data".to_string(), raw.clone());
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

        context.insert(
            "query_name".to_string(),
            Value::String(query_name.to_string()),
        );
        context.insert(
            "operation".to_string(),
            Value::String(operation_name.to_string()),
        );
        context
    }

    fn render_payload(
        handlebars: &Handlebars<'_>,
        config: &AzureStorageReactionConfig,
        query_name: &str,
        operation: OperationType,
        context: &Map<String, Value>,
        fallback_data: &Value,
    ) -> Result<String> {
        let template = Self::resolve_template_spec(config, query_name, operation)
            .map(|s| s.template.clone())
            .unwrap_or_default();

        if template.is_empty() {
            return Ok(serde_json::to_string(fallback_data)?);
        }

        Ok(handlebars.render_template(&template, context)?)
    }

    fn parse_table_properties(payload: &str) -> Result<Map<String, Value>> {
        let value: Value = serde_json::from_str(payload)
            .with_context(|| format!("table payload is not valid JSON object: {payload}"))?;
        let obj = value
            .as_object()
            .cloned()
            .context("table payload must be a JSON object")?;
        Ok(obj)
    }

    fn get_table_default_data(
        operation_name: &str,
        context: &Map<String, Value>,
        raw: &Value,
    ) -> Value {
        match operation_name {
            "ADD" => context.get("after").cloned().unwrap_or_else(|| raw.clone()),
            "UPDATE" => context
                .get("after")
                .cloned()
                .or_else(|| context.get("data").cloned())
                .unwrap_or_else(|| raw.clone()),
            _ => raw.clone(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_one(
        service_client: &ServiceClient,
        target: &StorageTarget,
        config: &AzureStorageReactionConfig,
        handlebars: &Handlebars<'_>,
        operation_name: &str,
        operation: OperationType,
        query_name: &str,
        data: &Value,
    ) -> Result<()> {
        let context = Self::build_context(operation_name, query_name, data);
        let payload =
            Self::render_payload(handlebars, config, query_name, operation, &context, data)?;

        match (service_client, target) {
            (
                ServiceClient::Blob(blob_service),
                StorageTarget::Blob {
                    container_name,
                    blob_path_template,
                    content_type,
                },
            ) => {
                let blob_path = handlebars.render_template(blob_path_template, &context)?;
                if operation == OperationType::Delete {
                    blob_service.delete_blob(container_name, &blob_path).await?;
                } else {
                    blob_service
                        .put_blob(container_name, &blob_path, &payload, content_type)
                        .await?;
                }
            }
            (ServiceClient::Queue(queue_service), StorageTarget::Queue { queue_name }) => {
                queue_service.send_message(queue_name, &payload).await?;
            }
            (
                ServiceClient::Table(table_service),
                StorageTarget::Table {
                    table_name,
                    partition_key_template,
                    row_key_template,
                },
            ) => {
                let partition_key = handlebars.render_template(partition_key_template, &context)?;
                let row_key = handlebars.render_template(row_key_template, &context)?;
                if operation == OperationType::Delete {
                    table_service
                        .delete_entity(table_name, &partition_key, &row_key)
                        .await?;
                } else {
                    let table_data = if Self::resolve_template_spec(config, query_name, operation)
                        .is_some_and(|spec| !spec.template.is_empty())
                    {
                        Self::parse_table_properties(&payload)?
                    } else {
                        let fallback = Self::get_table_default_data(operation_name, &context, data);
                        fallback
                            .as_object()
                            .cloned()
                            .context("table fallback payload must be JSON object")?
                    };

                    table_service
                        .upsert_entity(table_name, &partition_key, &row_key, table_data)
                        .await?;
                }
            }
            _ => anyhow::bail!("storage target and service client mismatch"),
        }

        Ok(())
    }
}

#[async_trait]
impl Reaction for AzureStorageReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "azure-storage"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "account_name".to_string(),
            Value::String(self.config.account_name.clone()),
        );
        props.insert(
            "target_type".to_string(),
            Value::String(
                match self.config.target {
                    StorageTarget::Blob { .. } => "blob",
                    StorageTarget::Queue { .. } => "queue",
                    StorageTarget::Table { .. } => "table",
                }
                .to_string(),
            ),
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
        log_component_start("Azure Storage Reaction", &self.base.id);
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting Azure Storage reaction".to_string()),
            )
            .await?;

        let credentials = azure_storage::StorageCredentials::access_key(
            self.config.account_name.clone(),
            self.config.access_key.clone(),
        );

        let service_client = match self.config.target {
            StorageTarget::Blob { .. } => ServiceClient::Blob(BlobService::new(
                &self.config.account_name,
                credentials.clone(),
                self.config.blob_endpoint.as_deref(),
            )),
            StorageTarget::Queue { .. } => ServiceClient::Queue(QueueService::new(
                &self.config.account_name,
                credentials.clone(),
                self.config.queue_endpoint.as_deref(),
            )),
            StorageTarget::Table { .. } => ServiceClient::Table(TableService::new(
                &self.config.account_name,
                credentials,
                self.config.table_endpoint.as_deref(),
            )),
        };

        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("Azure Storage reaction started".to_string()),
            )
            .await?;

        let mut shutdown_rx = self.base.create_shutdown_channel().await;
        let priority_queue = self.base.priority_queue.clone();
        let status = self.base.status.clone();
        let config = self.config.clone();
        let target = self.config.target.clone();
        let reaction_name = self.base.id.clone();

        let processing_task = tokio::spawn(async move {
            let mut handlebars = Handlebars::new();
            handlebars.register_helper(
                "json",
                Box::new(
                    |h: &handlebars::Helper<'_>,
                     _: &Handlebars<'_>,
                     _: &handlebars::Context,
                     _: &mut handlebars::RenderContext<'_, '_>,
                     out: &mut dyn handlebars::Output|
                     -> handlebars::HelperResult {
                        if let Some(value) = h.param(0) {
                            out.write(&serde_json::to_string(value.value()).unwrap_or_default())?;
                        }
                        Ok(())
                    },
                ),
            );

            loop {
                let query_result_arc = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_name}] Shutdown signal received");
                        break;
                    }
                    result = priority_queue.dequeue() => result,
                };
                let query_result = query_result_arc.as_ref();

                if !matches!(*status.read().await, ComponentStatus::Running) {
                    break;
                }

                for result in &query_result.results {
                    let op_and_data: Result<Option<(OperationType, &str, Value)>> = match result {
                        ResultDiff::Add { data } => {
                            Ok(Some((OperationType::Add, "ADD", data.clone())))
                        }
                        ResultDiff::Update { .. } => match serde_json::to_value(result) {
                            Ok(payload) => Ok(Some((OperationType::Update, "UPDATE", payload))),
                            Err(err) => Err(anyhow::anyhow!(
                                "failed to serialize UPDATE ResultDiff into JSON payload: {err}"
                            )),
                        },
                        ResultDiff::Delete { data } => {
                            Ok(Some((OperationType::Delete, "DELETE", data.clone())))
                        }
                        ResultDiff::Aggregation { .. } | ResultDiff::Noop => Ok(None),
                    };

                    let Some((operation, operation_name, data)) = (match op_and_data {
                        Ok(value) => value,
                        Err(err) => {
                            warn!(
                                "[{reaction_name}] Failed to prepare payload for query {}: {}",
                                query_result.query_id, err
                            );
                            continue;
                        }
                    }) else {
                        continue;
                    };

                    if let Err(err) = Self::process_one(
                        &service_client,
                        &target,
                        &config,
                        &handlebars,
                        operation_name,
                        operation,
                        &query_result.query_id,
                        &data,
                    )
                    .await
                    {
                        warn!(
                            "[{reaction_name}] Failed processing {} operation for query {}: {}",
                            operation_name, query_result.query_id, err
                        );
                    }
                }
            }

            info!("[{reaction_name}] Azure Storage reaction stopped");
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
                Some("Azure Storage reaction stopped".to_string()),
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
