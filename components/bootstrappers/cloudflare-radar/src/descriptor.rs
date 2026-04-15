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

//! Cloudflare Radar bootstrap plugin descriptor and configuration DTOs.

use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::CloudflareRadarBootstrapProvider;

// ── DTO types ────────────────────────────────────────────────────────────────

/// Category configuration DTO.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::cloudflare_radar::CategoryConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CategoryConfigDto {
    #[serde(default)]
    pub outages: bool,
    #[serde(default)]
    pub bgp_hijacks: bool,
    #[serde(default)]
    pub bgp_leaks: bool,
    #[serde(default)]
    pub http_traffic: bool,
    #[serde(default)]
    pub attacks_l7: bool,
    #[serde(default)]
    pub attacks_l3: bool,
    #[serde(default)]
    pub domain_rankings: bool,
    #[serde(default)]
    pub dns: bool,
}

fn default_api_base_url() -> ConfigValue<String> {
    ConfigValue::Static("https://api.cloudflare.com/client/v4".to_string())
}

fn default_ranking_limit() -> ConfigValue<u32> {
    ConfigValue::Static(100)
}

fn default_analytics_date_range() -> ConfigValue<String> {
    ConfigValue::Static("1d".to_string())
}

fn default_event_date_range() -> ConfigValue<String> {
    ConfigValue::Static("7d".to_string())
}

/// Cloudflare Radar bootstrap configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::cloudflare_radar::CloudflareRadarBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CloudflareRadarBootstrapConfigDto {
    #[schema(value_type = ConfigValueString)]
    pub api_token: ConfigValue<String>,
    #[serde(default = "default_api_base_url")]
    #[schema(value_type = ConfigValueString)]
    pub api_base_url: ConfigValue<String>,
    #[serde(default)]
    #[schema(value_type = bootstrap::cloudflare_radar::CategoryConfig)]
    pub categories: CategoryConfigDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location_filter: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asn_filter: Option<Vec<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hijack_min_confidence: Option<u32>,
    #[serde(default = "default_ranking_limit")]
    #[schema(value_type = ConfigValueU32)]
    pub ranking_limit: ConfigValue<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_domains: Option<Vec<String>>,
    #[serde(default = "default_analytics_date_range")]
    #[schema(value_type = ConfigValueString)]
    pub analytics_date_range: ConfigValue<String>,
    #[serde(default = "default_event_date_range")]
    #[schema(value_type = ConfigValueString)]
    pub event_date_range: ConfigValue<String>,
}

// ── Descriptor ───────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(CloudflareRadarBootstrapConfigDto, CategoryConfigDto,)))]
struct CloudflareRadarBootstrapSchemas;

/// Plugin descriptor for the Cloudflare Radar bootstrap provider.
pub struct CloudflareRadarBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for CloudflareRadarBootstrapDescriptor {
    fn kind(&self) -> &str {
        "cloudflare-radar"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.cloudflare_radar.CloudflareRadarBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = CloudflareRadarBootstrapSchemas::openapi();
        match api
            .components
            .as_ref()
            .and_then(|components| serde_json::to_string(&components.schemas).ok())
        {
            Some(schema) => schema,
            None => {
                log::warn!(
                    "Cloudflare Radar bootstrap schema generation failed; returning empty schema"
                );
                "{}".to_string()
            }
        }
    }

    async fn create_bootstrap_provider(
        &self,
        config_json: &serde_json::Value,
        _source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        let dto: CloudflareRadarBootstrapConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = CloudflareRadarBootstrapProvider::builder()
            .with_api_token(mapper.resolve_string(&dto.api_token)?)
            .with_api_base_url(mapper.resolve_string(&dto.api_base_url)?)
            .with_ranking_limit(mapper.resolve_typed(&dto.ranking_limit)?)
            .with_analytics_date_range(mapper.resolve_string(&dto.analytics_date_range)?)
            .with_event_date_range(mapper.resolve_string(&dto.event_date_range)?);

        builder = builder.with_category("outages", dto.categories.outages);
        builder = builder.with_category("bgp_hijacks", dto.categories.bgp_hijacks);
        builder = builder.with_category("bgp_leaks", dto.categories.bgp_leaks);
        builder = builder.with_category("http_traffic", dto.categories.http_traffic);
        builder = builder.with_category("attacks_l7", dto.categories.attacks_l7);
        builder = builder.with_category("attacks_l3", dto.categories.attacks_l3);
        builder = builder.with_category("domain_rankings", dto.categories.domain_rankings);
        builder = builder.with_category("dns", dto.categories.dns);

        if let Some(locations) = dto.location_filter {
            builder = builder.with_location_filter(locations);
        }
        if let Some(asns) = dto.asn_filter {
            builder = builder.with_bgp_asn_filter(asns);
        }
        if let Some(min_conf) = dto.hijack_min_confidence {
            builder = builder.with_hijack_min_confidence(min_conf);
        }
        if let Some(domains) = dto.dns_domains {
            builder = builder.with_dns_domains(domains);
        }

        let provider = builder.build()?;
        Ok(Box::new(provider))
    }
}
