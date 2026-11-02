use std::fmt;

use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::{SalesforceSource, SalesforceSourceConfig};
use drasi_salesforce_common::{
    InitialCursorBehavior, SalesforceAuthConfig, SalesforceConnectionConfig,
};

fn default_api_version() -> ConfigValue<String> {
    ConfigValue::Static("68.0".to_string())
}

fn default_pubsub_endpoint() -> ConfigValue<String> {
    ConfigValue::Static("https://api.pubsub.salesforce.com:7443".to_string())
}

fn default_reconcile_interval_seconds() -> ConfigValue<u64> {
    ConfigValue::Static(5)
}

fn default_full_scan_interval_seconds() -> ConfigValue<u64> {
    ConfigValue::Static(60)
}

fn default_max_rows() -> ConfigValue<usize> {
    ConfigValue::Static(100_000)
}

fn default_max_events() -> ConfigValue<usize> {
    ConfigValue::Static(1_000_000)
}

fn default_retention_hours() -> ConfigValue<u64> {
    ConfigValue::Static(96)
}

fn default_max_projected_row_bytes() -> ConfigValue<usize> {
    ConfigValue::Static(1_048_576)
}

fn default_max_scan_bytes() -> ConfigValue<usize> {
    ConfigValue::Static(8 * 1024 * 1024)
}

fn default_max_replay_page_events() -> ConfigValue<usize> {
    ConfigValue::Static(100)
}

fn default_max_replay_page_bytes() -> ConfigValue<usize> {
    ConfigValue::Static(1_048_576)
}

fn default_max_subscribers() -> ConfigValue<usize> {
    ConfigValue::Static(16)
}

fn default_snapshot_ticket_ttl_seconds() -> ConfigValue<u64> {
    ConfigValue::Static(600)
}

fn default_max_snapshot_leases() -> ConfigValue<usize> {
    ConfigValue::Static(16)
}

fn default_max_spool_bytes() -> ConfigValue<u64> {
    ConfigValue::Static(8 * 1024 * 1024 * 1024)
}

fn default_initial_cursor_behavior() -> ConfigValue<InitialCursorBehaviorDto> {
    ConfigValue::Static(InitialCursorBehaviorDto::Latest)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = source::salesforce::InitialCursorBehavior)]
#[serde(rename_all = "camelCase")]
pub enum InitialCursorBehaviorDto {
    Latest,
    Earliest,
}

impl std::str::FromStr for InitialCursorBehaviorDto {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "latest" => Ok(Self::Latest),
            "earliest" => Ok(Self::Earliest),
            _ => Err(anyhow::anyhow!(
                "invalid initial_cursor_behavior '{value}', expected 'latest' or 'earliest'"
            )),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::salesforce::SalesforceAuth)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SalesforceAuthConfigDto {
    ClientCredentials {
        #[serde(rename = "tokenUrl", alias = "token_url")]
        token_url: ConfigValue<String>,
        #[serde(rename = "clientId", alias = "client_id")]
        client_id: ConfigValue<String>,
        #[serde(rename = "clientSecret", alias = "client_secret")]
        client_secret: ConfigValue<String>,
    },
}

impl fmt::Debug for SalesforceAuthConfigDto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClientCredentials {
                token_url,
                client_id,
                ..
            } => f
                .debug_struct("ClientCredentials")
                .field("token_url", token_url)
                .field("client_id", client_id)
                .field("client_secret", &"<redacted>")
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::salesforce::SalesforceConnection)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SalesforceConnectionConfigDto {
    pub instance_url: ConfigValue<String>,
    pub expected_org_id: ConfigValue<String>,
    #[serde(default = "default_api_version")]
    pub api_version: ConfigValue<String>,
    pub auth: SalesforceAuthConfigDto,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::salesforce::SalesforceSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SalesforceSourceConfigDto {
    pub connection: SalesforceConnectionConfigDto,
    pub objects: Vec<ConfigValue<String>>,
    #[serde(default = "default_pubsub_endpoint")]
    pub pubsub_endpoint: ConfigValue<String>,
    #[serde(default = "default_reconcile_interval_seconds")]
    pub reconcile_interval_seconds: ConfigValue<u64>,
    #[serde(default = "default_full_scan_interval_seconds")]
    pub full_scan_interval_seconds: ConfigValue<u64>,
    #[serde(default = "default_max_rows")]
    pub max_rows: ConfigValue<usize>,
    #[serde(default = "default_max_events")]
    pub max_events: ConfigValue<usize>,
    #[serde(default = "default_retention_hours")]
    pub retention_hours: ConfigValue<u64>,
    #[serde(default = "default_max_projected_row_bytes")]
    pub max_projected_row_bytes: ConfigValue<usize>,
    #[serde(default = "default_max_scan_bytes")]
    pub max_scan_bytes: ConfigValue<usize>,
    #[serde(default = "default_max_replay_page_events")]
    pub max_replay_page_events: ConfigValue<usize>,
    #[serde(default = "default_max_replay_page_bytes")]
    pub max_replay_page_bytes: ConfigValue<usize>,
    #[serde(default = "default_max_subscribers")]
    pub max_subscribers: ConfigValue<usize>,
    pub spool_root: ConfigValue<String>,
    #[serde(default = "default_snapshot_ticket_ttl_seconds")]
    pub snapshot_ticket_ttl_seconds: ConfigValue<u64>,
    #[serde(default = "default_max_snapshot_leases")]
    pub max_snapshot_leases: ConfigValue<usize>,
    #[serde(default = "default_max_spool_bytes")]
    pub max_spool_bytes: ConfigValue<u64>,
    #[serde(default = "default_initial_cursor_behavior")]
    pub initial_cursor_behavior: ConfigValue<InitialCursorBehaviorDto>,
}

impl From<&SalesforceSourceConfig> for SalesforceSourceConfigDto {
    fn from(config: &SalesforceSourceConfig) -> Self {
        Self {
            connection: SalesforceConnectionConfigDto {
                instance_url: ConfigValue::Static(config.connection.instance_url.clone()),
                expected_org_id: ConfigValue::Static(config.connection.expected_org_id.clone()),
                api_version: ConfigValue::Static(config.connection.api_version.clone()),
                auth: match &config.connection.auth {
                    SalesforceAuthConfig::ClientCredentials {
                        token_url,
                        client_id,
                        client_secret,
                    } => SalesforceAuthConfigDto::ClientCredentials {
                        token_url: ConfigValue::Static(token_url.clone()),
                        client_id: ConfigValue::Static(client_id.clone()),
                        client_secret: ConfigValue::Static(client_secret.clone()),
                    },
                },
            },
            objects: config
                .objects
                .iter()
                .cloned()
                .map(ConfigValue::Static)
                .collect(),
            pubsub_endpoint: ConfigValue::Static(config.pubsub_endpoint.clone()),
            reconcile_interval_seconds: ConfigValue::Static(config.reconcile_interval_seconds),
            full_scan_interval_seconds: ConfigValue::Static(config.full_scan_interval_seconds),
            max_rows: ConfigValue::Static(config.max_rows),
            max_events: ConfigValue::Static(config.max_events),
            retention_hours: ConfigValue::Static(config.retention_hours),
            max_projected_row_bytes: ConfigValue::Static(config.max_projected_row_bytes),
            max_scan_bytes: ConfigValue::Static(config.max_scan_bytes),
            max_replay_page_events: ConfigValue::Static(config.max_replay_page_events),
            max_replay_page_bytes: ConfigValue::Static(config.max_replay_page_bytes),
            max_subscribers: ConfigValue::Static(config.max_subscribers),
            spool_root: ConfigValue::Static(config.spool_root.clone()),
            snapshot_ticket_ttl_seconds: ConfigValue::Static(config.snapshot_ticket_ttl_seconds),
            max_snapshot_leases: ConfigValue::Static(config.max_snapshot_leases),
            max_spool_bytes: ConfigValue::Static(config.max_spool_bytes),
            initial_cursor_behavior: ConfigValue::Static(match config.initial_cursor_behavior {
                InitialCursorBehavior::Latest => InitialCursorBehaviorDto::Latest,
                InitialCursorBehavior::Earliest => InitialCursorBehaviorDto::Earliest,
            }),
        }
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    SalesforceSourceConfigDto,
    SalesforceConnectionConfigDto,
    SalesforceAuthConfigDto,
    InitialCursorBehaviorDto
)))]
struct SalesforceSourceSchemas;

pub struct SalesforceSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for SalesforceSourceDescriptor {
    fn kind(&self) -> &str {
        "salesforce"
    }

    fn config_version(&self) -> &str {
        "2.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.salesforce.SalesforceSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = SalesforceSourceSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("failed to serialize Salesforce source schema")
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        anyhow::ensure!(
            config_json.get("projection").is_none(),
            "Salesforce configuration v1 is unsupported: replace projection with objects and use a new v2 spool and query state directory; existing state is preserved"
        );
        let dto: SalesforceSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let connection = SalesforceConnectionConfig {
            instance_url: mapper.resolve_string(&dto.connection.instance_url).await?,
            expected_org_id: mapper
                .resolve_string(&dto.connection.expected_org_id)
                .await?,
            api_version: mapper.resolve_string(&dto.connection.api_version).await?,
            auth: match dto.connection.auth {
                SalesforceAuthConfigDto::ClientCredentials {
                    token_url,
                    client_id,
                    client_secret,
                } => SalesforceAuthConfig::ClientCredentials {
                    token_url: mapper.resolve_string(&token_url).await?,
                    client_id: mapper.resolve_string(&client_id).await?,
                    client_secret: mapper.resolve_string(&client_secret).await?,
                },
            },
        };

        let mut objects = Vec::with_capacity(dto.objects.len());
        for object in &dto.objects {
            objects.push(mapper.resolve_string(object).await?);
        }

        let initial_cursor_behavior = match mapper
            .resolve_typed::<InitialCursorBehaviorDto>(&dto.initial_cursor_behavior)
            .await?
        {
            InitialCursorBehaviorDto::Latest => InitialCursorBehavior::Latest,
            InitialCursorBehaviorDto::Earliest => InitialCursorBehavior::Earliest,
        };

        let config = SalesforceSourceConfig {
            connection,
            objects,
            pubsub_endpoint: mapper.resolve_string(&dto.pubsub_endpoint).await?,
            reconcile_interval_seconds: mapper
                .resolve_typed(&dto.reconcile_interval_seconds)
                .await?,
            full_scan_interval_seconds: mapper
                .resolve_typed(&dto.full_scan_interval_seconds)
                .await?,
            max_rows: mapper.resolve_typed(&dto.max_rows).await?,
            max_events: mapper.resolve_typed(&dto.max_events).await?,
            retention_hours: mapper.resolve_typed(&dto.retention_hours).await?,
            max_projected_row_bytes: mapper.resolve_typed(&dto.max_projected_row_bytes).await?,
            max_scan_bytes: mapper.resolve_typed(&dto.max_scan_bytes).await?,
            max_replay_page_events: mapper.resolve_typed(&dto.max_replay_page_events).await?,
            max_replay_page_bytes: mapper.resolve_typed(&dto.max_replay_page_bytes).await?,
            max_subscribers: mapper.resolve_typed(&dto.max_subscribers).await?,
            spool_root: mapper.resolve_string(&dto.spool_root).await?,
            snapshot_ticket_ttl_seconds: mapper
                .resolve_typed(&dto.snapshot_ticket_ttl_seconds)
                .await?,
            max_snapshot_leases: mapper.resolve_typed(&dto.max_snapshot_leases).await?,
            max_spool_bytes: mapper.resolve_typed(&dto.max_spool_bytes).await?,
            initial_cursor_behavior,
        };

        let mut source = SalesforceSource::builder(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;
        source.base.set_raw_config(config_json.clone());
        Ok(Box::new(source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auth_debug_redacts_client_secret() {
        let auth = SalesforceAuthConfigDto::ClientCredentials {
            token_url: ConfigValue::Static("https://example.test/oauth/token".to_string()),
            client_id: ConfigValue::Static("client-id".to_string()),
            client_secret: ConfigValue::Static("super-secret".to_string()),
        };

        let debug = format!("{auth:?}");
        assert!(
            !debug.contains("super-secret"),
            "debug output leaked client secret: {debug}"
        );
        assert!(
            debug.contains("<redacted>"),
            "debug output should explicitly redact secret values: {debug}"
        );
    }

    #[test]
    fn auth_dto_deserializes_camel_case_fields() {
        let auth: SalesforceAuthConfigDto = serde_json::from_value(json!({
            "type": "clientCredentials",
            "tokenUrl": "https://example.test/oauth/token",
            "clientId": "fixture-client-id",
            "clientSecret": "fixture-client-secret"
        }))
        .expect("camelCase auth DTO should deserialize");

        match auth {
            SalesforceAuthConfigDto::ClientCredentials {
                token_url,
                client_id,
                client_secret,
            } => {
                assert_eq!(
                    token_url,
                    ConfigValue::Static("https://example.test/oauth/token".to_string())
                );
                assert_eq!(
                    client_id,
                    ConfigValue::Static("fixture-client-id".to_string())
                );
                assert_eq!(
                    client_secret,
                    ConfigValue::Static("fixture-client-secret".to_string())
                );
            }
        }
    }

    #[test]
    fn auth_dto_serializes_fields_as_camel_case() {
        let auth = SalesforceAuthConfigDto::ClientCredentials {
            token_url: ConfigValue::Static("https://example.test/oauth/token".to_string()),
            client_id: ConfigValue::Static("fixture-client-id".to_string()),
            client_secret: ConfigValue::Static("fixture-client-secret".to_string()),
        };

        let value = serde_json::to_value(auth).expect("auth DTO serialization should succeed");
        let object = value
            .as_object()
            .expect("serialized auth DTO should be a JSON object");
        assert!(object.contains_key("type"));
        assert!(object.contains_key("tokenUrl"));
        assert!(object.contains_key("clientId"));
        assert!(object.contains_key("clientSecret"));
        assert!(!object.contains_key("token_url"));
        assert!(!object.contains_key("client_id"));
        assert!(!object.contains_key("client_secret"));
    }
}
