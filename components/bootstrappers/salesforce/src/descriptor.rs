use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::SalesforceBootstrapProvider;

fn default_max_rows() -> ConfigValue<usize> {
    ConfigValue::Static(100_000)
}

fn default_max_row_bytes() -> ConfigValue<usize> {
    ConfigValue::Static(1_048_576)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::salesforce::SalesforceBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SalesforceBootstrapConfigDto {
    pub spool_root: ConfigValue<String>,
    #[serde(default = "default_max_rows")]
    pub max_rows: ConfigValue<usize>,
    #[serde(default = "default_max_row_bytes")]
    pub max_row_bytes: ConfigValue<usize>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(SalesforceBootstrapConfigDto)))]
struct SalesforceBootstrapSchemas;

pub struct SalesforceBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for SalesforceBootstrapDescriptor {
    fn kind(&self) -> &str {
        "salesforce"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.salesforce.SalesforceBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = SalesforceBootstrapSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("failed to serialize Salesforce bootstrap schema")
    }

    async fn create_bootstrap_provider(
        &self,
        config_json: &serde_json::Value,
        _source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        let dto: SalesforceBootstrapConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let provider = SalesforceBootstrapProvider::builder()
            .with_spool_root(mapper.resolve_string(&dto.spool_root).await?)
            .with_max_rows(mapper.resolve_typed(&dto.max_rows).await?)
            .with_max_row_bytes(mapper.resolve_typed(&dto.max_row_bytes).await?)
            .build()?;

        Ok(Box::new(provider))
    }
}
