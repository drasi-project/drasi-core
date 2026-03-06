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

//! Descriptor for Azure Storage reaction plugin.

use std::collections::HashMap;

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::{AzureStorageReaction, QueryConfig, StorageTarget, TemplateSpec};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::azure_storage::TemplateSpec)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TemplateSpecDto {
    pub template: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::azure_storage::AzureQueryConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AzureQueryConfigDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<TemplateSpecDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<TemplateSpecDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<TemplateSpecDto>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::azure_storage::StorageTarget)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "camelCase")]
pub enum StorageTargetDto {
    Blob {
        container_name: String,
        blob_path_template: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content_type: Option<String>,
    },
    Queue {
        queue_name: String,
    },
    Table {
        table_name: String,
        partition_key_template: String,
        row_key_template: String,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::azure_storage::AzureStorageReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct AzureStorageReactionConfigDto {
    #[schema(value_type = ConfigValueString)]
    pub account_name: ConfigValue<String>,
    #[schema(value_type = ConfigValueString)]
    pub access_key: ConfigValue<String>,
    pub target: StorageTargetDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub blob_endpoint: Option<ConfigValue<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub queue_endpoint: Option<ConfigValue<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub table_endpoint: Option<ConfigValue<String>>,
    #[serde(default)]
    pub routes: HashMap<String, AzureQueryConfigDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<AzureQueryConfigDto>,
}

fn map_template_spec(dto: &TemplateSpecDto) -> TemplateSpec {
    TemplateSpec::new(dto.template.clone())
}

fn map_query_config(dto: &AzureQueryConfigDto) -> QueryConfig {
    QueryConfig {
        added: dto.added.as_ref().map(map_template_spec),
        updated: dto.updated.as_ref().map(map_template_spec),
        deleted: dto.deleted.as_ref().map(map_template_spec),
    }
}

fn map_target(dto: &StorageTargetDto) -> StorageTarget {
    match dto {
        StorageTargetDto::Blob {
            container_name,
            blob_path_template,
            content_type,
        } => StorageTarget::Blob {
            container_name: container_name.clone(),
            blob_path_template: blob_path_template.clone(),
            content_type: content_type
                .clone()
                .unwrap_or_else(crate::config::default_blob_content_type),
        },
        StorageTargetDto::Queue { queue_name } => StorageTarget::Queue {
            queue_name: queue_name.clone(),
        },
        StorageTargetDto::Table {
            table_name,
            partition_key_template,
            row_key_template,
        } => StorageTarget::Table {
            table_name: table_name.clone(),
            partition_key_template: partition_key_template.clone(),
            row_key_template: row_key_template.clone(),
        },
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    AzureStorageReactionConfigDto,
    StorageTargetDto,
    AzureQueryConfigDto,
    TemplateSpecDto
)))]
struct AzureStorageReactionSchemas;

pub struct AzureStorageReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for AzureStorageReactionDescriptor {
    fn kind(&self) -> &str {
        "azure-storage"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.azure_storage.AzureStorageReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = AzureStorageReactionSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("failed to serialize schema")
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: AzureStorageReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = AzureStorageReaction::builder(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_account_name(mapper.resolve_string(&dto.account_name)?)
            .with_access_key(mapper.resolve_string(&dto.access_key)?)
            .with_target(map_target(&dto.target));

        if let Some(v) = &dto.blob_endpoint {
            builder = builder.with_blob_endpoint(mapper.resolve_string(v)?);
        }
        if let Some(v) = &dto.queue_endpoint {
            builder = builder.with_queue_endpoint(mapper.resolve_string(v)?);
        }
        if let Some(v) = &dto.table_endpoint {
            builder = builder.with_table_endpoint(mapper.resolve_string(v)?);
        }

        if let Some(default_template) = &dto.default_template {
            builder = builder.with_default_template(map_query_config(default_template));
        }
        for (query_id, query_config) in &dto.routes {
            builder = builder.with_route(query_id, map_query_config(query_config));
        }

        Ok(Box::new(builder.build()?))
    }
}
