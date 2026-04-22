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

//! HERE Traffic source plugin descriptor and configuration DTOs.

use crate::config::{AuthMethod, Endpoint, StartFrom};
use crate::HereTrafficSourceBuilder;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = source::here_traffic::Endpoint)]
#[serde(rename_all = "lowercase")]
pub enum EndpointDto {
    Flow,
    Incidents,
}

impl From<EndpointDto> for Endpoint {
    fn from(dto: EndpointDto) -> Self {
        match dto {
            EndpointDto::Flow => Endpoint::Flow,
            EndpointDto::Incidents => Endpoint::Incidents,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema, Default)]
#[schema(as = source::here_traffic::StartFrom)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StartFromDto {
    #[default]
    Now,
    Timestamp {
        value: i64,
    },
}

impl std::str::FromStr for StartFromDto {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_lowercase();
        if normalized == "now" {
            return Ok(StartFromDto::Now);
        }

        if let Some(stripped) = normalized.strip_prefix("timestamp:") {
            return stripped
                .parse::<i64>()
                .map(|timestamp| StartFromDto::Timestamp { value: timestamp })
                .map_err(|e| format!("Invalid timestamp value: {e}"));
        }

        normalized
            .parse::<i64>()
            .map(|timestamp| StartFromDto::Timestamp { value: timestamp })
            .map_err(|_| format!("Invalid start_from value: {value}"))
    }
}

impl From<StartFromDto> for StartFrom {
    fn from(dto: StartFromDto) -> Self {
        match dto {
            StartFromDto::Now => StartFrom::Now,
            StartFromDto::Timestamp { value } => StartFrom::Timestamp { value },
        }
    }
}

/// Source configuration DTO.
///
/// Supports two authentication modes (mutually exclusive):
/// - **API key**: set `apiKey` only
/// - **OAuth 2.0**: set `accessKeyId` and `accessKeySecret`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::here_traffic::HereTrafficSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HereTrafficSourceConfigDto {
    /// Simple API key (mutually exclusive with OAuth fields).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub api_key: Option<ConfigValue<String>>,
    /// OAuth 2.0 access key ID (requires accessKeySecret).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub access_key_id: Option<ConfigValue<String>>,
    /// OAuth 2.0 access key secret (requires accessKeyId).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub access_key_secret: Option<ConfigValue<String>>,
    /// OAuth 2.0 token endpoint URL (defaults to HERE production).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub token_url: Option<ConfigValue<String>>,
    #[schema(value_type = ConfigValueString)]
    pub bounding_box: ConfigValue<String>,
    #[serde(default = "default_polling_interval")]
    pub polling_interval: ConfigValue<u64>,
    #[serde(default = "default_endpoints")]
    #[schema(value_type = Vec<source::here_traffic::Endpoint>)]
    pub endpoints: Vec<EndpointDto>,
    #[serde(default = "default_flow_change_threshold")]
    pub flow_change_threshold: ConfigValue<f64>,
    #[serde(default = "default_speed_change_threshold")]
    pub speed_change_threshold: ConfigValue<f64>,
    #[serde(default = "default_relation_distance_meters")]
    pub incident_match_distance_meters: ConfigValue<f64>,
    #[serde(default = "default_start_from")]
    #[schema(value_type = source::here_traffic::StartFrom)]
    pub start_from: ConfigValue<StartFromDto>,
}

fn default_polling_interval() -> ConfigValue<u64> {
    ConfigValue::Static(60)
}

fn default_endpoints() -> Vec<EndpointDto> {
    vec![EndpointDto::Flow, EndpointDto::Incidents]
}

fn default_flow_change_threshold() -> ConfigValue<f64> {
    ConfigValue::Static(0.5)
}

fn default_speed_change_threshold() -> ConfigValue<f64> {
    ConfigValue::Static(5.0)
}

fn default_relation_distance_meters() -> ConfigValue<f64> {
    ConfigValue::Static(500.0)
}

fn default_start_from() -> ConfigValue<StartFromDto> {
    ConfigValue::Static(StartFromDto::default())
}

/// Resolve the auth method from DTO fields.
fn resolve_auth(
    dto: &HereTrafficSourceConfigDto,
    mapper: &DtoMapper,
) -> anyhow::Result<AuthMethod> {
    let has_api_key = dto.api_key.is_some();
    let has_oauth = dto.access_key_id.is_some() || dto.access_key_secret.is_some();

    if has_api_key && has_oauth {
        return Err(anyhow::anyhow!(
            "Cannot specify both apiKey and accessKeyId/accessKeySecret; choose one auth method"
        ));
    }

    if has_oauth {
        let id = dto
            .access_key_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("accessKeyId is required for OAuth"))?;
        let secret = dto
            .access_key_secret
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("accessKeySecret is required for OAuth"))?;

        let access_key_id = mapper.resolve_string(id)?;
        let access_key_secret = mapper.resolve_string(secret)?;
        let token_url = if let Some(url) = &dto.token_url {
            mapper.resolve_string(url)?
        } else {
            "https://account.api.here.com/oauth2/token".to_string()
        };

        return Ok(AuthMethod::OAuth {
            access_key_id,
            access_key_secret,
            token_url,
        });
    }

    if let Some(api_key_cfg) = &dto.api_key {
        let api_key = mapper.resolve_string(api_key_cfg)?;
        return Ok(AuthMethod::ApiKey { api_key });
    }

    Err(anyhow::anyhow!(
        "Either apiKey or accessKeyId+accessKeySecret must be provided"
    ))
}

#[derive(OpenApi)]
#[openapi(components(schemas(HereTrafficSourceConfigDto, EndpointDto, StartFromDto,)))]
struct HereTrafficSourceSchemas;

pub struct HereTrafficSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for HereTrafficSourceDescriptor {
    fn kind(&self) -> &str {
        "here-traffic"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.here_traffic.HereTrafficSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = HereTrafficSourceSchemas::openapi();
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
        let dto: HereTrafficSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let auth = resolve_auth(&dto, &mapper)?;
        let bounding_box = mapper.resolve_string(&dto.bounding_box)?;
        let polling_interval = mapper.resolve_typed(&dto.polling_interval)?;
        let flow_change_threshold = mapper.resolve_typed(&dto.flow_change_threshold)?;
        let speed_change_threshold = mapper.resolve_typed(&dto.speed_change_threshold)?;
        let incident_match_distance_meters =
            mapper.resolve_typed(&dto.incident_match_distance_meters)?;
        let start_from: StartFromDto = mapper.resolve_typed(&dto.start_from)?;

        let endpoints = dto.endpoints.into_iter().map(Endpoint::from).collect();

        let mut config = match &auth {
            AuthMethod::ApiKey { api_key } => {
                crate::HereTrafficConfig::new(api_key.clone(), bounding_box)
            }
            AuthMethod::OAuth {
                access_key_id,
                access_key_secret,
                ..
            } => crate::HereTrafficConfig::new_oauth(
                access_key_id.clone(),
                access_key_secret.clone(),
                bounding_box,
            ),
        };
        config.auth = auth;
        config.polling_interval = polling_interval;
        config.endpoints = endpoints;
        config.flow_change_threshold = flow_change_threshold;
        config.speed_change_threshold = speed_change_threshold;
        config.incident_match_distance_meters = incident_match_distance_meters;
        config.start_from = start_from.into();

        let source = HereTrafficSourceBuilder::from_config(id, config)
            .with_auto_start(auto_start)
            .build()?;

        Ok(Box::new(source))
    }
}
