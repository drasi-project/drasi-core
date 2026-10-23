// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

//! Host configuration references and dynamic-plugin discovery.

use drasi_lib::Reaction;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::{config::*, TemporalReaction};

/// Host-resolved PEM client identity.
#[derive(Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::temporal::ClientIdentity)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ClientIdentityDto {
    #[schema(value_type = ConfigValueString)]
    pub certificate: ConfigValue<String>,
    #[schema(value_type = ConfigValueString)]
    pub private_key: ConfigValue<String>,
}

/// Host-resolved TLS settings.
#[derive(Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::temporal::TlsConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TlsConfigDto {
    #[schema(value_type = Option<ConfigValueString>)]
    pub server_root_ca_cert: Option<ConfigValue<String>>,
    #[schema(value_type = Option<ConfigValueString>)]
    pub domain: Option<ConfigValue<String>>,
    pub client_identity: Option<ClientIdentityDto>,
}

/// Declarative configuration. Sensitive values are intentionally not Debug.
#[derive(Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::temporal::TemporalReactionConfig)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct TemporalReactionConfigDto {
    #[schema(value_type = ConfigValueString)]
    pub endpoint: ConfigValue<String>,
    #[schema(value_type = ConfigValueString)]
    pub namespace: ConfigValue<String>,
    #[schema(value_type = ConfigValueU64)]
    pub timeout_ms: ConfigValue<u64>,
    #[schema(value_type = ConfigValueU64)]
    pub priority_queue_capacity: ConfigValue<u64>,
    pub output_templates: OutputTemplatesDto,
    #[schema(value_type = Option<ConfigValueString>)]
    pub api_key: Option<ConfigValue<String>>,
    pub tls: Option<TlsConfigDto>,
}

impl Default for TemporalReactionConfigDto {
    fn default() -> Self {
        Self::from(&TemporalReactionConfig::default())
    }
}

impl From<&TemporalReactionConfig> for TemporalReactionConfigDto {
    fn from(config: &TemporalReactionConfig) -> Self {
        Self {
            endpoint: ConfigValue::Static(config.endpoint.clone()),
            namespace: ConfigValue::Static(config.namespace.clone()),
            timeout_ms: ConfigValue::Static(config.timeout_ms),
            priority_queue_capacity: ConfigValue::Static(config.priority_queue_capacity as u64),
            output_templates: config.output_templates.clone().into(),
            api_key: config.api_key.clone().map(ConfigValue::Static),
            tls: config.tls.as_ref().map(|tls| TlsConfigDto {
                server_root_ca_cert: tls.server_root_ca_cert.clone().map(ConfigValue::Static),
                domain: tls.domain.clone().map(ConfigValue::Static),
                client_identity: tls
                    .client_identity
                    .as_ref()
                    .map(|identity| ClientIdentityDto {
                        certificate: ConfigValue::Static(identity.certificate.clone()),
                        private_key: ConfigValue::Static(identity.private_key.clone()),
                    }),
            }),
        }
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    TemporalReactionConfigDto,
    OutputTemplatesDto,
    QueryActionsDto,
    ActionSpecDto,
    Action,
    ConflictPolicy,
    ReusePolicy,
    UpdateWait,
    TlsConfigDto,
    ClientIdentityDto
)))]
struct Schemas;

/// Registers the `temporal` kind, configuration schema, and factory.
pub struct TemporalReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for TemporalReactionDescriptor {
    fn kind(&self) -> &str {
        "temporal"
    }
    fn config_version(&self) -> &str {
        "1.0.0"
    }
    fn config_schema_name(&self) -> &str {
        "reaction.temporal.TemporalReactionConfig"
    }
    fn display_name(&self) -> &str {
        "Temporal"
    }
    fn display_description(&self) -> &str {
        "Maps query diffs to Temporal signals, starts, Updates, cancellations, or terminations."
    }
    fn display_icon(&self) -> &str {
        "workflow"
    }
    fn config_schema_json(&self) -> String {
        let api = Schemas::openapi();
        let schemas = serde_json::to_value(&api.components.expect("schema components").schemas)
            .expect("serialize schemas");
        drasi_plugin_sdk::schema_ui::SchemaUiAnnotator::new(schemas, self.config_schema_name())
            .expect("Temporal schema exists")
            .field("endpoint", |f| f.group("Connection").order(1))
            .field("namespace", |f| f.group("Connection").order(2))
            .field("apiKey", |f| {
                f.group("Connection").order(3).widget("password")
            })
            .field("tls", |f| f.group("Connection").order(4))
            .field("outputTemplates", |f| f.group("Actions").order(10))
            .annotate()
            .to_string()
    }
    async fn create_reaction(
        &self,
        id: &str,
        queries: Vec<String>,
        raw: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: TemporalReactionConfigDto = serde_json::from_value(raw.clone())?;
        let mapper = DtoMapper::new();
        let tls = match dto.tls {
            Some(tls) => Some(TlsConfig {
                server_root_ca_cert: match tls.server_root_ca_cert {
                    Some(v) => Some(mapper.resolve_string(&v).await?),
                    None => None,
                },
                domain: match tls.domain {
                    Some(v) => Some(mapper.resolve_string(&v).await?),
                    None => None,
                },
                client_identity: match tls.client_identity {
                    Some(v) => Some(ClientIdentity {
                        certificate: mapper.resolve_string(&v.certificate).await?,
                        private_key: mapper.resolve_string(&v.private_key).await?,
                    }),
                    None => None,
                },
            }),
            None => None,
        };
        let capacity: u64 = mapper.resolve_typed(&dto.priority_queue_capacity).await?;
        let config = TemporalReactionConfig {
            endpoint: mapper.resolve_string(&dto.endpoint).await?,
            namespace: mapper.resolve_string(&dto.namespace).await?,
            timeout_ms: mapper.resolve_typed(&dto.timeout_ms).await?,
            priority_queue_capacity: usize::try_from(capacity)?,
            output_templates: dto.output_templates.into(),
            api_key: match dto.api_key {
                Some(v) => Some(mapper.resolve_string(&v).await?),
                None => None,
            },
            tls,
        };
        let mut reaction = TemporalReaction::builder(id)
            .with_queries(queries)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;
        reaction.base.set_raw_config(raw.clone());
        Ok(Box::new(reaction))
    }
}
