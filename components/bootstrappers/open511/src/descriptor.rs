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

//! Open511 bootstrap provider descriptor.

use crate::{Open511BootstrapConfig, Open511BootstrapProviderBuilder};
use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

/// Open511 bootstrap configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::open511::Open511BootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Open511BootstrapConfigDto {
    pub base_url: ConfigValue<String>,
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: ConfigValue<u64>,
    #[serde(default = "default_page_size")]
    pub page_size: ConfigValue<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_filter: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity_filter: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_type_filter: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area_id_filter: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub road_name_filter: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jurisdiction_filter: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox_filter: Option<ConfigValue<String>>,
}

fn default_request_timeout_secs() -> ConfigValue<u64> {
    ConfigValue::Static(15)
}

fn default_page_size() -> ConfigValue<usize> {
    ConfigValue::Static(500)
}

#[derive(OpenApi)]
#[openapi(components(schemas(Open511BootstrapConfigDto)))]
struct Open511BootstrapSchemas;

/// Descriptor for Open511 bootstrap provider.
pub struct Open511BootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for Open511BootstrapDescriptor {
    fn kind(&self) -> &str {
        "open511"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.open511.Open511BootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = Open511BootstrapSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_bootstrap_provider(
        &self,
        config_json: &serde_json::Value,
        _source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        let dto: Open511BootstrapConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let builder = Open511BootstrapProviderBuilder::new()
            .with_base_url(mapper.resolve_string(&dto.base_url)?)
            .with_request_timeout_secs(mapper.resolve_typed(&dto.request_timeout_secs)?)
            .with_page_size(mapper.resolve_typed(&dto.page_size)?);

        let builder = if let Some(status_filter) = dto.status_filter {
            builder.with_status_filter(mapper.resolve_string(&status_filter)?)
        } else {
            builder
        };
        let builder = if let Some(severity_filter) = dto.severity_filter {
            builder.with_severity_filter(mapper.resolve_string(&severity_filter)?)
        } else {
            builder
        };
        let builder = if let Some(event_type_filter) = dto.event_type_filter {
            builder.with_event_type_filter(mapper.resolve_string(&event_type_filter)?)
        } else {
            builder
        };
        let builder = if let Some(area_id_filter) = dto.area_id_filter {
            builder.with_area_id_filter(mapper.resolve_string(&area_id_filter)?)
        } else {
            builder
        };
        let builder = if let Some(road_name_filter) = dto.road_name_filter {
            builder.with_road_name_filter(mapper.resolve_string(&road_name_filter)?)
        } else {
            builder
        };
        let builder = if let Some(jurisdiction_filter) = dto.jurisdiction_filter {
            builder.with_jurisdiction_filter(mapper.resolve_string(&jurisdiction_filter)?)
        } else {
            builder
        };
        let builder = if let Some(bbox_filter) = dto.bbox_filter {
            builder.with_bbox_filter(mapper.resolve_string(&bbox_filter)?)
        } else {
            builder
        };

        Ok(Box::new(builder.build()?))
    }
}
