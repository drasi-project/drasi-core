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

//! OpenTelemetry source plugin descriptor and configuration DTOs.

use crate::{OtelSourceBuilder, OtelSourceConfig};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

/// OpenTelemetry source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::otel::OtelSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OtelSourceConfigDto {
    #[serde(default = "default_grpc_bind")]
    pub grpc_bind: ConfigValue<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_bind: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert_path: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key_path: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_ca_path: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<ConfigValue<String>>,
    #[serde(default)]
    pub metric_allowlist: Vec<String>,
    #[serde(default)]
    pub metric_identity_attributes: Vec<String>,
    #[serde(default = "default_destination_attributes")]
    pub destination_attributes: Vec<String>,
    #[serde(default = "default_span_kinds")]
    pub span_kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_metric: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_event_name: Option<ConfigValue<String>>,
    #[serde(default = "default_log_min_severity")]
    pub log_min_severity: ConfigValue<String>,
    #[serde(default)]
    pub log_event_name_allowlist: Vec<String>,
    #[serde(default = "default_log_event_ttl_secs")]
    pub log_event_ttl_secs: ConfigValue<u64>,
    #[serde(default = "default_dependency_ttl_secs")]
    pub dependency_ttl_secs: ConfigValue<u64>,
    #[serde(default = "default_max_services")]
    pub max_services: ConfigValue<usize>,
    #[serde(default = "default_max_metrics")]
    pub max_metrics: ConfigValue<usize>,
    #[serde(default = "default_max_dependencies")]
    pub max_dependencies: ConfigValue<usize>,
    #[serde(default = "default_max_log_events")]
    pub max_log_events: ConfigValue<usize>,
    #[serde(default = "default_reject_derived")]
    pub reject_derived: ConfigValue<bool>,
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: ConfigValue<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durability: Option<drasi_lib::DurabilityConfig>,
}

fn default_grpc_bind() -> ConfigValue<String> {
    ConfigValue::Static(crate::config::default_grpc_bind())
}

fn default_destination_attributes() -> Vec<String> {
    vec!["peer.service".to_string()]
}

fn default_span_kinds() -> Vec<String> {
    vec!["CLIENT".to_string()]
}

fn default_log_min_severity() -> ConfigValue<String> {
    ConfigValue::Static("ERROR".to_string())
}

fn default_log_event_ttl_secs() -> ConfigValue<u64> {
    ConfigValue::Static(60)
}

fn default_dependency_ttl_secs() -> ConfigValue<u64> {
    ConfigValue::Static(300)
}

fn default_max_services() -> ConfigValue<usize> {
    ConfigValue::Static(1000)
}

fn default_max_metrics() -> ConfigValue<usize> {
    ConfigValue::Static(2000)
}

fn default_max_dependencies() -> ConfigValue<usize> {
    ConfigValue::Static(5000)
}

fn default_max_log_events() -> ConfigValue<usize> {
    ConfigValue::Static(5000)
}

fn default_reject_derived() -> ConfigValue<bool> {
    ConfigValue::Static(true)
}

fn default_max_request_bytes() -> ConfigValue<usize> {
    ConfigValue::Static(4 * 1024 * 1024)
}

#[derive(OpenApi)]
#[openapi(components(schemas(OtelSourceConfigDto)))]
struct OtelSourceSchemas;

/// Descriptor for the OpenTelemetry source plugin.
pub struct OtelSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for OtelSourceDescriptor {
    fn kind(&self) -> &str {
        "otel"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.otel.OtelSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = OtelSourceSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: OtelSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let config = OtelSourceConfig {
            grpc_bind: mapper.resolve_string(&dto.grpc_bind).await?,
            http_bind: mapper.resolve_optional(&dto.http_bind).await?,
            tls_cert_path: mapper.resolve_optional(&dto.tls_cert_path).await?,
            tls_key_path: mapper.resolve_optional(&dto.tls_key_path).await?,
            tls_client_ca_path: mapper.resolve_optional(&dto.tls_client_ca_path).await?,
            auth_token: mapper.resolve_optional(&dto.auth_token).await?,
            metric_allowlist: dto.metric_allowlist,
            metric_identity_attributes: dto.metric_identity_attributes,
            destination_attributes: dto.destination_attributes,
            span_kinds: dto.span_kinds,
            heartbeat_metric: mapper.resolve_optional(&dto.heartbeat_metric).await?,
            heartbeat_event_name: mapper.resolve_optional(&dto.heartbeat_event_name).await?,
            log_min_severity: mapper.resolve_string(&dto.log_min_severity).await?,
            log_event_name_allowlist: dto.log_event_name_allowlist,
            log_event_ttl_secs: mapper.resolve_typed(&dto.log_event_ttl_secs).await?,
            dependency_ttl_secs: mapper.resolve_typed(&dto.dependency_ttl_secs).await?,
            max_services: mapper.resolve_typed(&dto.max_services).await?,
            max_metrics: mapper.resolve_typed(&dto.max_metrics).await?,
            max_dependencies: mapper.resolve_typed(&dto.max_dependencies).await?,
            max_log_events: mapper.resolve_typed(&dto.max_log_events).await?,
            reject_derived: mapper.resolve_typed(&dto.reject_derived).await?,
            max_request_bytes: mapper.resolve_typed(&dto.max_request_bytes).await?,
            durability: dto.durability.clone(),
        };

        let mut source = OtelSourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;
        source.base_mut().set_raw_config(config_json.clone());
        Ok(Box::new(source))
    }
}
