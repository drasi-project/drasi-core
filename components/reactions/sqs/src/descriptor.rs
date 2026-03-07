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

//! Descriptor for the SQS reaction plugin.

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

use crate::{QueryConfig, SqsReactionBuilder, TemplateSpec};

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::sqs::TemplateSpec)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TemplateSpecDto {
    /// SQS message body template.
    #[serde(default)]
    pub body: String,
    /// Additional message attributes with Handlebars-rendered values.
    #[serde(default)]
    pub message_attributes: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::sqs::SqsQueryConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SqsQueryConfigDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<TemplateSpecDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<TemplateSpecDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<TemplateSpecDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::sqs::SqsReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct SqsReactionConfigDto {
    /// SQS queue URL.
    #[schema(value_type = ConfigValueString)]
    pub queue_url: ConfigValue<String>,

    /// Optional region override.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub region: Option<ConfigValue<String>>,

    /// Optional SQS endpoint URL override.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub endpoint_url: Option<ConfigValue<String>>,

    /// Whether the target queue is FIFO.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueBool>)]
    pub fifo_queue: Option<ConfigValue<bool>>,

    /// Optional template used to produce FIFO MessageGroupId.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub message_group_id_template: Option<ConfigValue<String>>,

    /// Optional static AWS access key ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub access_key_id: Option<ConfigValue<String>>,

    /// Optional static AWS secret access key.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub secret_access_key: Option<ConfigValue<String>>,

    /// Query-specific operation templates.
    #[serde(default)]
    pub routes: HashMap<String, SqsQueryConfigDto>,

    /// Default template route when query-specific route does not exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<SqsQueryConfigDto>,
}

fn map_template_spec(dto: &TemplateSpecDto) -> TemplateSpec {
    TemplateSpec {
        body: dto.body.clone(),
        message_attributes: dto.message_attributes.clone(),
    }
}

fn map_query_config(dto: &SqsQueryConfigDto) -> QueryConfig {
    QueryConfig {
        added: dto.added.as_ref().map(map_template_spec),
        updated: dto.updated.as_ref().map(map_template_spec),
        deleted: dto.deleted.as_ref().map(map_template_spec),
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(SqsReactionConfigDto, SqsQueryConfigDto, TemplateSpecDto,)))]
struct SqsReactionSchemas;

pub struct SqsReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for SqsReactionDescriptor {
    fn kind(&self) -> &str {
        "sqs"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.sqs.SqsReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = SqsReactionSchemas::openapi();
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
        let dto: SqsReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = SqsReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_queue_url(mapper.resolve_string(&dto.queue_url)?);

        if let Some(region) = &dto.region {
            builder = builder.with_region(mapper.resolve_string(region)?);
        }
        if let Some(endpoint_url) = &dto.endpoint_url {
            builder = builder.with_endpoint_url(mapper.resolve_string(endpoint_url)?);
        }
        if let Some(fifo_queue) = &dto.fifo_queue {
            builder = builder.with_fifo_queue(mapper.resolve_typed(fifo_queue)?);
        }
        if let Some(group_id_template) = &dto.message_group_id_template {
            builder =
                builder.with_message_group_id_template(mapper.resolve_string(group_id_template)?);
        }
        if let Some(access_key_id) = &dto.access_key_id {
            if let Some(secret_access_key) = &dto.secret_access_key {
                builder = builder.with_credentials(
                    mapper.resolve_string(access_key_id)?,
                    mapper.resolve_string(secret_access_key)?,
                );
            }
        }
        if let Some(default_template) = &dto.default_template {
            builder = builder.with_default_template(map_query_config(default_template));
        }
        for (query_id, route) in &dto.routes {
            builder = builder.with_route(query_id, map_query_config(route));
        }

        Ok(Box::new(builder.build()?))
    }
}
