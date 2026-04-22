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

//! Cloudflare Radar source plugin descriptor and configuration DTOs.

use crate::{CloudflareRadarSourceBuilder, StartBehavior};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

// ── DTO types ────────────────────────────────────────────────────────────────

/// Category configuration DTO.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::cloudflare_radar::CategoryConfig)]
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

/// Start behavior DTO.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = source::cloudflare_radar::StartBehavior)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StartBehaviorDto {
    #[default]
    StartFromNow,
    StartFromBeginning,
    StartFromTimestamp {
        timestamp: i64,
    },
}

fn default_api_base_url() -> ConfigValue<String> {
    ConfigValue::Static("https://api.cloudflare.com/client/v4".to_string())
}

fn default_poll_interval_secs() -> ConfigValue<u64> {
    ConfigValue::Static(300)
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

fn default_start_behavior() -> StartBehaviorDto {
    StartBehaviorDto::default()
}

/// Cloudflare Radar source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::cloudflare_radar::CloudflareRadarConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CloudflareRadarConfigDto {
    #[schema(value_type = ConfigValueString)]
    pub api_token: ConfigValue<String>,
    #[serde(default = "default_api_base_url")]
    #[schema(value_type = ConfigValueString)]
    pub api_base_url: ConfigValue<String>,
    #[serde(default = "default_poll_interval_secs")]
    #[schema(value_type = ConfigValueU64)]
    pub poll_interval_secs: ConfigValue<u64>,
    #[serde(default)]
    #[schema(value_type = source::cloudflare_radar::CategoryConfig)]
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
    #[serde(default = "default_start_behavior")]
    #[schema(value_type = source::cloudflare_radar::StartBehavior)]
    pub start_behavior: StartBehaviorDto,
}

// ── Descriptor ───────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(CloudflareRadarConfigDto, CategoryConfigDto, StartBehaviorDto,)))]
struct CloudflareRadarSchemas;

/// Plugin descriptor for the Cloudflare Radar source.
pub struct CloudflareRadarSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for CloudflareRadarSourceDescriptor {
    fn kind(&self) -> &str {
        "cloudflare-radar"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.cloudflare_radar.CloudflareRadarConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = CloudflareRadarSchemas::openapi();
        match api
            .components
            .as_ref()
            .and_then(|components| serde_json::to_string(&components.schemas).ok())
        {
            Some(schema) => schema,
            None => {
                log::warn!(
                    "Cloudflare Radar source schema generation failed; returning empty schema"
                );
                "{}".to_string()
            }
        }
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: CloudflareRadarConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let start_behavior = match dto.start_behavior {
            StartBehaviorDto::StartFromNow => StartBehavior::StartFromNow,
            StartBehaviorDto::StartFromBeginning => StartBehavior::StartFromBeginning,
            StartBehaviorDto::StartFromTimestamp { timestamp } => {
                StartBehavior::StartFromTimestamp(timestamp)
            }
        };

        let mut builder = CloudflareRadarSourceBuilder::new(id)
            .with_api_token(mapper.resolve_string(&dto.api_token)?)
            .with_api_base_url(mapper.resolve_string(&dto.api_base_url)?)
            .with_poll_interval_secs(mapper.resolve_typed(&dto.poll_interval_secs)?)
            .with_ranking_limit(mapper.resolve_typed(&dto.ranking_limit)?)
            .with_analytics_date_range(mapper.resolve_string(&dto.analytics_date_range)?)
            .with_event_date_range(mapper.resolve_string(&dto.event_date_range)?)
            .with_start_behavior(start_behavior)
            .with_auto_start(auto_start);

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

        let source = builder.build()?;
        Ok(Box::new(source))
    }
}
