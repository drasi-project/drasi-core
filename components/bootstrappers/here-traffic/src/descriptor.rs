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

//! Plugin descriptor for the HERE Traffic bootstrap provider.

use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use drasi_source_here_traffic::{AuthMethod, Endpoint, HereTrafficConfig};

use crate::HereTrafficBootstrapProvider;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = bootstrap::here_traffic::Endpoint)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::here_traffic::HereTrafficBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HereTrafficBootstrapConfigDto {
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
    #[serde(default = "default_endpoints")]
    #[schema(value_type = Vec<bootstrap::here_traffic::Endpoint>)]
    pub endpoints: Vec<EndpointDto>,
    #[serde(default = "default_relation_distance_meters")]
    pub incident_match_distance_meters: ConfigValue<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub base_url: Option<ConfigValue<String>>,
}

fn default_endpoints() -> Vec<EndpointDto> {
    vec![EndpointDto::Flow, EndpointDto::Incidents]
}

fn default_relation_distance_meters() -> ConfigValue<f64> {
    ConfigValue::Static(500.0)
}

#[derive(OpenApi)]
#[openapi(components(schemas(HereTrafficBootstrapConfigDto, EndpointDto,)))]
struct HereTrafficBootstrapSchemas;

pub struct HereTrafficBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for HereTrafficBootstrapDescriptor {
    fn kind(&self) -> &str {
        "here-traffic"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.here_traffic.HereTrafficBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = HereTrafficBootstrapSchemas::openapi();
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
        let dto: HereTrafficBootstrapConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let auth = resolve_bootstrap_auth(&dto, &mapper)?;
        let bounding_box = mapper.resolve_string(&dto.bounding_box)?;
        let endpoints = dto.endpoints.into_iter().map(Endpoint::from).collect();
        let incident_match_distance_meters =
            mapper.resolve_typed(&dto.incident_match_distance_meters)?;

        let mut config = match &auth {
            AuthMethod::ApiKey { api_key } => HereTrafficConfig::new(api_key.clone(), bounding_box),
            AuthMethod::OAuth {
                access_key_id,
                access_key_secret,
                ..
            } => HereTrafficConfig::new_oauth(
                access_key_id.clone(),
                access_key_secret.clone(),
                bounding_box,
            ),
        };
        config.auth = auth;
        config.endpoints = endpoints;
        config.incident_match_distance_meters = incident_match_distance_meters;
        if let Some(base_url) = dto.base_url {
            config.base_url = mapper.resolve_string(&base_url)?;
        }

        let provider = HereTrafficBootstrapProvider::new("here-traffic-bootstrap", config)?;
        Ok(Box::new(provider))
    }
}

fn resolve_bootstrap_auth(
    dto: &HereTrafficBootstrapConfigDto,
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
