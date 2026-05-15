#![allow(unexpected_cfgs)]
// Copyright 2026 The Drasi Authors.
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

//! Dataverse Bootstrap Provider for Drasi
//!
//! This plugin provides initial data loading from Microsoft Dataverse entities
//! using the OData Web API. It mirrors the platform's `BootstrapHandler` which
//! uses `RetrieveEntityChangesRequest` WITHOUT a `DataVersion` to fetch all
//! current records.
//!
//! # Web API Equivalent
//!
//! The platform's bootstrap handler uses:
//! ```csharp
//! var request = new RetrieveEntityChangesRequest {
//!     EntityName = entityName,
//!     Columns = new ColumnSet(true),
//!     PageInfo = new PagingInfo { Count = 200 }
//! };
//! ```
//!
//! This is equivalent to:
//! ```text
//! GET /api/data/v9.2/{entity_set}?$top=5000
//! ```
//! with pagination via `@odata.nextLink`.
//!
//! # Usage
//!
//! ```rust,ignore
//! use drasi_bootstrap_dataverse::DataverseBootstrapProvider;
//!
//! let provider = DataverseBootstrapProvider::builder()
//!     .with_environment_url("https://myorg.crm.dynamics.com")
//!     .with_tenant_id("tenant-id")
//!     .with_client_id("client-id")
//!     .with_client_secret("secret")
//!     .with_entities(vec!["account".to_string()])
//!     .build()
//!     .unwrap();
//! ```

pub mod config;
pub mod descriptor;

pub use config::DataverseBootstrapConfig;

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use log::{debug, info, warn};

use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use drasi_lib::identity::{Credentials, IdentityProvider};

/// Dataverse bootstrap provider for initial data loading.
///
/// This provider fetches all existing records from configured Dataverse entities
/// and sends them as `SourceChange::Insert` events. It uses the OData Web API
/// to page through records, matching the platform's `BootstrapHandler` behavior.
pub struct DataverseBootstrapProvider {
    config: DataverseBootstrapConfig,
    /// Optional identity provider. When set, takes precedence over
    /// config-based client credentials / Azure CLI for token acquisition.
    identity_provider: Option<Box<dyn IdentityProvider>>,
}

impl DataverseBootstrapProvider {
    /// Create a new bootstrap provider with a given configuration.
    pub fn new(config: DataverseBootstrapConfig) -> Result<Self> {
        config.validate().map_err(|e| anyhow::anyhow!(e))?;
        Ok(Self {
            config,
            identity_provider: None,
        })
    }

    /// Create a builder for constructing a bootstrap provider.
    pub fn builder() -> DataverseBootstrapProviderBuilder {
        DataverseBootstrapProviderBuilder::new()
    }

    /// Fetch an authentication token.
    ///
    /// Priority:
    /// 1. Identity provider (if set) — delegates to `get_credentials()`
    /// 2. Azure developer tools (CLI / azd / VS) when `use_azure_cli` is true
    /// 3. OAuth2 client credentials (service principal)
    ///
    /// The internal paths delegate to `drasi-identity-azure`'s
    /// `AzureIdentityProvider`, which wraps `azure_identity` from the Azure
    /// SDK for Rust. This keeps OAuth2 flow handling, token caching, and
    /// developer-tool integration in one well-maintained place rather than
    /// reimplementing them here.
    async fn get_token(&self) -> Result<String> {
        if let Some(ref provider) = self.identity_provider {
            return self.get_token_from_provider(provider.as_ref()).await;
        }

        let azure_provider = if self.config.use_azure_cli {
            drasi_identity_azure::AzureIdentityProvider::with_default_credentials("dataverse")?
                .with_scope(dataverse_scope(&self.config.environment_url))
        } else {
            drasi_identity_azure::AzureIdentityProvider::with_client_secret(
                "dataverse",
                &self.config.tenant_id,
                &self.config.client_id,
                &self.config.client_secret,
            )?
            .with_scope(dataverse_scope(&self.config.environment_url))
        };
        self.get_token_from_provider(&azure_provider).await
    }

    /// Acquire a Dataverse-scoped token from any `IdentityProvider`.
    async fn get_token_from_provider(&self, provider: &dyn IdentityProvider) -> Result<String> {
        let scope = dataverse_scope(&self.config.environment_url);
        let context = drasi_lib::identity::CredentialContext::new().with_property("scope", &scope);
        let creds = provider.get_credentials(&context).await?;
        match creds {
            Credentials::Token { token, .. } => Ok(token),
            _ => Err(anyhow::anyhow!(
                "Dataverse bootstrap requires Token credentials from identity provider"
            )),
        }
    }

    /// Fetch all records from an entity set, with pagination.
    ///
    /// Mirrors the platform's `BootstrapHandler` which iterates through all
    /// pages using `PagingCookie` and returns all `NewOrUpdatedItem` records.
    async fn fetch_entity_data(
        &self,
        http_client: &reqwest::Client,
        token: &str,
        entity_name: &str,
    ) -> Result<Vec<serde_json::Value>> {
        let entity_set = self.config.entity_set_name(entity_name);
        let select = self.config.select_columns(entity_name);

        let mut url = format!(
            "{}/api/data/{}/{}",
            self.config.environment_url, self.config.api_version, entity_set
        );

        // Add $select if configured
        let mut query_params = Vec::new();
        if let Some(ref cols) = select {
            query_params.push(format!("$select={cols}"));
        }
        query_params.push(format!("$top={}", self.config.page_size));

        if !query_params.is_empty() {
            url = format!("{}?{}", url, query_params.join("&"));
        }

        let mut all_records = Vec::new();
        let mut current_url = url;
        let mut page = 1;

        loop {
            debug!("Bootstrap: Fetching page {page} for entity {entity_name}");

            let resp = http_client
                .get(&current_url)
                .bearer_auth(token)
                .header("Accept", "application/json")
                .header("OData-MaxVersion", "4.0")
                .header("OData-Version", "4.0")
                .send()
                .await?
                .error_for_status()?;

            let body: serde_json::Value = resp.json().await?;

            // Extract records from response
            if let Some(records) = body.get("value").and_then(|v| v.as_array()) {
                debug!(
                    "Bootstrap: Got {} records on page {page} for entity {entity_name}",
                    records.len()
                );
                all_records.extend(records.clone());
            }

            // Check for next page
            if let Some(next_link) = body.get("@odata.nextLink").and_then(|v| v.as_str()) {
                current_url = next_link.to_string();
                page += 1;
            } else {
                break;
            }
        }

        info!(
            "Bootstrap: Fetched {} total records for entity {entity_name}",
            all_records.len()
        );
        Ok(all_records)
    }

    /// Convert a record JSON value to a SourceChange::Insert.
    ///
    /// Mirrors the platform's `JsonEventMapper.MapData` which converts each
    /// `IChangedItem` into a `SourceElement` with `ChangeOp.INSERT`.
    fn record_to_source_change(
        source_id: &str,
        entity_name: &str,
        record: &serde_json::Value,
    ) -> Option<SourceChange> {
        let obj = record.as_object()?;

        // Extract entity ID - try common Dataverse ID patterns
        let entity_id_key = format!("{entity_name}id");
        let id = obj
            .get(&entity_id_key)
            .or_else(|| obj.get("id"))
            .and_then(|v| v.as_str());
        let id = match id {
            Some(id) => id,
            None => {
                warn!(
                    "Skipping record for entity '{entity_name}' because no ID field ('{entity_id_key}' or 'id') was present"
                );
                return None;
            }
        };

        // Build properties from all non-annotation attributes
        let mut properties = ElementPropertyMap::new();
        for (key, value) in obj {
            // Skip OData annotations (prefixed with @ or containing @).
            // Other fields, including Dataverse lookup GUID fields such as
            // `_xxx_value`, are preserved as element properties.
            if key.starts_with('@') || key.contains('@') {
                continue;
            }
            let element_value = convert_json_value(value);
            properties.insert(key, element_value);
        }

        let element_id = format!("{entity_name}:{id}");
        let metadata = ElementMetadata {
            reference: ElementReference::new(source_id, &element_id),
            labels: Arc::from(vec![Arc::from(entity_name)]),
            effective_from: chrono::Utc::now().timestamp_millis().max(0) as u64,
        };

        Some(SourceChange::Insert {
            element: Element::Node {
                metadata,
                properties,
            },
        })
    }
}

#[async_trait]
impl BootstrapProvider for DataverseBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        info!(
            "Starting Dataverse bootstrap for query '{}' with {} node labels",
            request.query_id,
            request.node_labels.len()
        );

        let http_client = reqwest::Client::new();

        // Get authentication token
        let token = self.get_token().await?;

        // Determine which entities to bootstrap
        // Filter based on requested labels - if the query wants 'account', only bootstrap 'account'
        let entities_to_bootstrap: Vec<&str> = if request.node_labels.is_empty() {
            // If no labels specified, bootstrap all configured entities
            self.config.entities.iter().map(|s| s.as_str()).collect()
        } else {
            // Only bootstrap entities requested by the query
            self.config
                .entities
                .iter()
                .filter(|e| request.node_labels.contains(e))
                .map(|s| s.as_str())
                .collect()
        };

        if entities_to_bootstrap.is_empty() {
            warn!(
                "No matching entities to bootstrap for query '{}'. Configured entities: {:?}, requested labels: {:?}",
                request.query_id, self.config.entities, request.node_labels
            );
            return Ok(BootstrapResult {
                event_count: 0,
                ..Default::default()
            });
        }

        let mut total_count = 0;

        for entity_name in entities_to_bootstrap {
            info!("Bootstrap: Loading data for entity '{entity_name}'");

            let records = self
                .fetch_entity_data(&http_client, &token, entity_name)
                .await?;

            let mut batch_count = 0;
            for record in &records {
                if let Some(source_change) =
                    Self::record_to_source_change(&context.source_id, entity_name, record)
                {
                    let sequence = context.next_sequence();
                    let event = BootstrapEvent {
                        source_id: context.source_id.clone(),
                        change: source_change,
                        timestamp: chrono::Utc::now(),
                        sequence,
                    };

                    event_tx.send(event).await.map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to send bootstrap event for entity {entity_name}: {e}"
                        )
                    })?;

                    batch_count += 1;
                }
            }

            info!("Bootstrap: Sent {batch_count} records for entity '{entity_name}'");
            total_count += batch_count;
        }

        info!(
            "Completed Dataverse bootstrap for query '{}': sent {} total records",
            request.query_id, total_count
        );

        Ok(BootstrapResult {
            event_count: total_count,
            ..Default::default()
        })
    }
}

/// Compute the OAuth2 scope for a Dataverse environment URL.
///
/// Dataverse expects scopes of the form `<scheme>://<host>/.default`,
/// derived strictly from the environment URL's origin (any path or trailing
/// slash is dropped). When the URL fails to parse we fall back to
/// `<env>/.default`, mirroring previous behaviour.
fn dataverse_scope(environment_url: &str) -> String {
    match url::Url::parse(environment_url) {
        Ok(url) => match url.host_str() {
            Some(host) => format!("{}://{}/.default", url.scheme(), host),
            None => format!("{}/.default", environment_url.trim_end_matches('/')),
        },
        Err(_) => format!("{}/.default", environment_url.trim_end_matches('/')),
    }
}

/// Convert a JSON value to an ElementValue.
///
/// Handles Dataverse-specific value patterns:
/// - `{"Value": x}` → extracts inner value (OptionSetValue, Money, etc.)
/// - `[{"Value": 1}, {"Value": 2}]` → list of extracted values (MultiSelectOptionSet)
fn convert_json_value(value: &serde_json::Value) -> ElementValue {
    match value {
        serde_json::Value::Null => ElementValue::Null,
        serde_json::Value::Bool(b) => ElementValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                ElementValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                ElementValue::Float(ordered_float::OrderedFloat(f))
            } else {
                ElementValue::Null
            }
        }
        serde_json::Value::String(s) => ElementValue::String(Arc::from(s.as_str())),
        serde_json::Value::Array(arr) => {
            if !arr.is_empty() {
                if let Some(first_obj) = arr[0].as_object() {
                    if first_obj.contains_key("Value") {
                        let values: Vec<ElementValue> = arr
                            .iter()
                            .filter_map(|item| {
                                item.as_object()
                                    .and_then(|obj| obj.get("Value"))
                                    .map(convert_json_value)
                            })
                            .collect();
                        return ElementValue::List(values);
                    }
                }
            }
            ElementValue::List(arr.iter().map(convert_json_value).collect())
        }
        serde_json::Value::Object(obj) => {
            if obj.contains_key("Value") && obj.len() <= 2 {
                if let Some(val) = obj.get("Value") {
                    return convert_json_value(val);
                }
            }
            let mut map = ElementPropertyMap::new();
            for (k, v) in obj {
                map.insert(k, convert_json_value(v));
            }
            ElementValue::Object(map)
        }
    }
}

/// Builder for `DataverseBootstrapProvider`.
pub struct DataverseBootstrapProviderBuilder {
    environment_url: String,
    tenant_id: String,
    client_id: String,
    client_secret: String,
    use_azure_cli: bool,
    entities: Vec<String>,
    entity_set_overrides: HashMap<String, String>,
    entity_columns: HashMap<String, Vec<String>>,
    api_version: String,
    page_size: usize,
    identity_provider: Option<Box<dyn IdentityProvider>>,
}

impl DataverseBootstrapProviderBuilder {
    /// Create a new builder with default values.
    pub fn new() -> Self {
        Self {
            environment_url: String::new(),
            tenant_id: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            use_azure_cli: false,
            entities: Vec::new(),
            entity_set_overrides: HashMap::new(),
            entity_columns: HashMap::new(),
            api_version: "v9.2".to_string(),
            page_size: 5000,
            identity_provider: None,
        }
    }

    /// Set the Dataverse environment URL.
    pub fn with_environment_url(mut self, url: impl Into<String>) -> Self {
        self.environment_url = url.into();
        self
    }

    /// Set the Azure AD tenant ID.
    pub fn with_tenant_id(mut self, tenant_id: impl Into<String>) -> Self {
        self.tenant_id = tenant_id.into();
        self
    }

    /// Set the Azure AD client ID.
    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = client_id.into();
        self
    }

    /// Set the Azure AD client secret.
    pub fn with_client_secret(mut self, client_secret: impl Into<String>) -> Self {
        self.client_secret = client_secret.into();
        self
    }

    /// Use Azure CLI authentication instead of client credentials.
    pub fn with_azure_cli_auth(mut self) -> Self {
        self.use_azure_cli = true;
        self
    }

    /// Set an identity provider for token acquisition.
    ///
    /// When an identity provider is set, it takes precedence over
    /// `tenant_id`/`client_id`/`client_secret` and `use_azure_cli`.
    /// The provider's `get_credentials()` must return `Credentials::Token`.
    pub fn with_identity_provider(mut self, provider: impl IdentityProvider + 'static) -> Self {
        self.identity_provider = Some(Box::new(provider));
        self
    }

    /// Set the entity logical names to bootstrap.
    pub fn with_entities(mut self, entities: Vec<String>) -> Self {
        self.entities = entities;
        self
    }

    /// Add a single entity to bootstrap.
    pub fn with_entity(mut self, entity: impl Into<String>) -> Self {
        self.entities.push(entity.into());
        self
    }

    /// Override the entity set name for a specific entity.
    pub fn with_entity_set_override(
        mut self,
        entity_name: impl Into<String>,
        entity_set_name: impl Into<String>,
    ) -> Self {
        self.entity_set_overrides
            .insert(entity_name.into(), entity_set_name.into());
        self
    }

    /// Set column selection for a specific entity.
    pub fn with_entity_columns(mut self, entity: impl Into<String>, columns: Vec<String>) -> Self {
        self.entity_columns.insert(entity.into(), columns);
        self
    }

    /// Set the Web API version.
    pub fn with_api_version(mut self, version: impl Into<String>) -> Self {
        self.api_version = version.into();
        self
    }

    /// Set the page size for batch fetching.
    pub fn with_page_size(mut self, page_size: usize) -> Self {
        self.page_size = page_size;
        self
    }

    /// Build the `DataverseBootstrapProvider`.
    pub fn build(self) -> Result<DataverseBootstrapProvider> {
        let config = DataverseBootstrapConfig {
            environment_url: self.environment_url,
            tenant_id: self.tenant_id,
            client_id: self.client_id,
            client_secret: self.client_secret,
            use_azure_cli: self.use_azure_cli,
            entities: self.entities,
            entity_set_overrides: self.entity_set_overrides,
            entity_columns: self.entity_columns,
            api_version: self.api_version,
            page_size: self.page_size,
        };

        // When an identity provider is supplied, skip client credential validation
        if self.identity_provider.is_some() {
            config
                .validate_with_identity_provider()
                .map_err(|e| anyhow::anyhow!(e))?;
        } else {
            config.validate().map_err(|e| anyhow::anyhow!(e))?;
        }

        Ok(DataverseBootstrapProvider {
            config,
            identity_provider: self.identity_provider,
        })
    }
}

impl Default for DataverseBootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_creates_provider() {
        let provider = DataverseBootstrapProvider::builder()
            .with_environment_url("https://test.crm.dynamics.com")
            .with_tenant_id("t")
            .with_client_id("c")
            .with_client_secret("s")
            .with_entities(vec!["account".to_string()])
            .build();
        assert!(provider.is_ok());
    }

    #[test]
    fn test_builder_fails_without_url() {
        let provider = DataverseBootstrapProvider::builder()
            .with_tenant_id("t")
            .with_client_id("c")
            .with_client_secret("s")
            .with_entities(vec!["account".to_string()])
            .build();
        assert!(provider.is_err());
    }

    #[test]
    fn test_builder_fails_without_entities() {
        let provider = DataverseBootstrapProvider::builder()
            .with_environment_url("https://test.crm.dynamics.com")
            .with_tenant_id("t")
            .with_client_id("c")
            .with_client_secret("s")
            .build();
        assert!(provider.is_err());
    }

    #[test]
    fn test_builder_with_entity() {
        let provider = DataverseBootstrapProvider::builder()
            .with_environment_url("https://test.crm.dynamics.com")
            .with_tenant_id("t")
            .with_client_id("c")
            .with_client_secret("s")
            .with_entity("account")
            .with_entity("contact")
            .build()
            .expect("should build");
        assert_eq!(provider.config.entities, vec!["account", "contact"]);
    }

    #[test]
    fn test_builder_with_custom_page_size() {
        let provider = DataverseBootstrapProvider::builder()
            .with_environment_url("https://test.crm.dynamics.com")
            .with_tenant_id("t")
            .with_client_id("c")
            .with_client_secret("s")
            .with_entities(vec!["account".to_string()])
            .with_page_size(1000)
            .build()
            .expect("should build");
        assert_eq!(provider.config.page_size, 1000);
    }

    #[test]
    fn test_builder_with_identity_provider() {
        let identity = drasi_lib::identity::PasswordIdentityProvider::new("user", "token");
        let provider = DataverseBootstrapProvider::builder()
            .with_environment_url("https://test.crm.dynamics.com")
            .with_entities(vec!["account".to_string()])
            .with_identity_provider(identity)
            .build()
            .expect("should build with identity provider and no client credentials");
        assert!(provider.identity_provider.is_some());
    }

    #[test]
    fn test_builder_with_identity_provider_still_needs_url() {
        let identity = drasi_lib::identity::PasswordIdentityProvider::new("user", "token");
        let result = DataverseBootstrapProvider::builder()
            .with_entities(vec!["account".to_string()])
            .with_identity_provider(identity)
            .build();
        assert!(result.is_err(), "should fail without environment_url");
    }

    #[test]
    fn test_builder_with_identity_provider_still_needs_entities() {
        let identity = drasi_lib::identity::PasswordIdentityProvider::new("user", "token");
        let result = DataverseBootstrapProvider::builder()
            .with_environment_url("https://test.crm.dynamics.com")
            .with_identity_provider(identity)
            .build();
        assert!(result.is_err(), "should fail without entities");
    }

    #[test]
    fn test_new_with_valid_config() {
        let config = DataverseBootstrapConfig {
            environment_url: "https://test.crm.dynamics.com".to_string(),
            tenant_id: "t".to_string(),
            client_id: "c".to_string(),
            client_secret: "s".to_string(),
            entities: vec!["account".to_string()],
            ..Default::default()
        };
        let provider = DataverseBootstrapProvider::new(config);
        assert!(provider.is_ok());
    }

    #[test]
    fn test_record_to_source_change_basic() {
        let record = serde_json::json!({
            "accountid": "abc-123",
            "name": "Contoso Ltd",
            "revenue": 1000000.0,
            "@odata.etag": "W/\"12345\"",
            "createdon@OData.Community.Display.V1.FormattedValue": "1/1/2024"
        });

        let change =
            DataverseBootstrapProvider::record_to_source_change("test-source", "account", &record);

        assert!(change.is_some());
        match change.unwrap() {
            SourceChange::Insert { element } => match element {
                Element::Node {
                    metadata,
                    properties,
                } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "account:abc-123");
                    assert_eq!(metadata.reference.source_id.as_ref(), "test-source");
                    assert_eq!(metadata.labels.len(), 1);
                    assert_eq!(metadata.labels[0].as_ref(), "account");
                    // Should have name, revenue, accountid but NOT OData annotations
                    assert!(properties.get("name").is_some());
                    assert!(properties.get("revenue").is_some());
                    assert!(properties.get("accountid").is_some());
                    // OData annotations should be filtered out
                    assert!(properties.get("@odata.etag").is_none());
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Insert change"),
        }
    }

    #[test]
    fn test_record_to_source_change_with_id_fallback() {
        let record = serde_json::json!({
            "id": "def-456",
            "name": "Test Record"
        });

        let change = DataverseBootstrapProvider::record_to_source_change(
            "test-source",
            "customentity",
            &record,
        );

        assert!(change.is_some());
        match change.unwrap() {
            SourceChange::Insert { element } => match element {
                Element::Node { metadata, .. } => {
                    assert_eq!(
                        metadata.reference.element_id.as_ref(),
                        "customentity:def-456"
                    );
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Insert change"),
        }
    }

    #[test]
    fn test_convert_json_value_primitives() {
        assert_eq!(
            convert_json_value(&serde_json::Value::Null),
            ElementValue::Null
        );
        assert_eq!(
            convert_json_value(&serde_json::json!(true)),
            ElementValue::Bool(true)
        );
        assert_eq!(
            convert_json_value(&serde_json::json!(42)),
            ElementValue::Integer(42)
        );
        assert_eq!(
            convert_json_value(&serde_json::json!("hello")),
            ElementValue::String(Arc::from("hello"))
        );
    }

    #[test]
    fn test_convert_json_value_extracts_value_wrapper() {
        let json = serde_json::json!({"Value": 123});
        assert_eq!(convert_json_value(&json), ElementValue::Integer(123));
    }

    mod validate {
        use super::*;

        #[test]
        fn rejects_missing_environment_url() {
            let result = DataverseBootstrapProvider::builder()
                .with_tenant_id("t")
                .with_client_id("c")
                .with_client_secret("s")
                .with_entities(vec!["account".to_string()])
                .build();
            let err = match result {
                Ok(_) => panic!("missing environment_url should fail validation"),
                Err(e) => e,
            };
            assert!(format!("{err}").contains("environment_url"));
        }

        #[test]
        fn rejects_missing_entities() {
            let result = DataverseBootstrapProvider::builder()
                .with_environment_url("https://test.crm.dynamics.com")
                .with_tenant_id("t")
                .with_client_id("c")
                .with_client_secret("s")
                .build();
            let err = match result {
                Ok(_) => panic!("missing entities should fail validation"),
                Err(e) => e,
            };
            assert!(format!("{err}").contains("entit"));
        }

        #[test]
        fn rejects_missing_credentials_when_no_identity_provider() {
            // No tenant/client/secret AND no identity provider AND no Azure CLI mode.
            let result = DataverseBootstrapProvider::builder()
                .with_environment_url("https://test.crm.dynamics.com")
                .with_entities(vec!["account".to_string()])
                .build();
            assert!(result.is_err(), "should require credentials");
        }
    }

    /// Bootstrap end-to-end tests using `wiremock` to simulate the Dataverse
    /// OData Web API. These verify that the provider:
    /// - Pages through `@odata.nextLink` until exhausted
    /// - Emits one `SourceChange::Insert` per fetched record
    /// - Filters entities based on `BootstrapRequest.node_labels`
    mod bootstrap_e2e {
        use super::*;
        use async_trait::async_trait;
        use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
        use drasi_lib::channels::BootstrapEvent;
        use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};
        use serde_json::json;
        use tokio::sync::mpsc;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        /// Identity provider that returns a fixed token to bypass real OAuth2.
        struct StaticToken;

        #[async_trait]
        impl IdentityProvider for StaticToken {
            async fn get_credentials(
                &self,
                _ctx: &CredentialContext,
            ) -> anyhow::Result<Credentials> {
                Ok(Credentials::Token {
                    username: "test".to_string(),
                    token: "mock-token".to_string(),
                })
            }
            fn clone_box(&self) -> Box<dyn IdentityProvider> {
                Box::new(StaticToken)
            }
        }

        fn make_request(query_id: &str, labels: Vec<String>) -> BootstrapRequest {
            BootstrapRequest {
                query_id: query_id.to_string(),
                node_labels: labels,
                relation_labels: Vec::new(),
                request_id: "req-1".to_string(),
            }
        }

        async fn collect_events(mut rx: mpsc::Receiver<BootstrapEvent>) -> Vec<BootstrapEvent> {
            let mut out = Vec::new();
            while let Some(ev) = rx.recv().await {
                out.push(ev);
            }
            out
        }

        #[tokio::test]
        async fn pages_through_odata_next_link() {
            let server = MockServer::start().await;
            let uri = server.uri();

            // Page 1 → returns nextLink to /accounts/page2 with 2 records
            Mock::given(method("GET"))
                .and(path("/api/data/v9.2/accounts"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": [
                        { "accountid": "id-1", "name": "One" },
                        { "accountid": "id-2", "name": "Two" }
                    ],
                    "@odata.nextLink": format!("{uri}/api/data/v9.2/accounts/page2")
                })))
                .mount(&server)
                .await;

            // Page 2 → returns 2 more records, no nextLink (terminating page)
            Mock::given(method("GET"))
                .and(path("/api/data/v9.2/accounts/page2"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": [
                        { "accountid": "id-3", "name": "Three" },
                        { "accountid": "id-4", "name": "Four" }
                    ]
                })))
                .mount(&server)
                .await;

            let provider = DataverseBootstrapProvider::builder()
                .with_environment_url(&uri)
                .with_entities(vec!["account".to_string()])
                .with_identity_provider(StaticToken)
                .build()
                .expect("provider should build");

            let (tx, rx) = mpsc::channel(16);
            let ctx =
                BootstrapContext::new_minimal("test-server".to_string(), "test-source".to_string());
            let request = make_request("q-1", Vec::new());

            let result = provider
                .bootstrap(request, &ctx, tx, None)
                .await
                .expect("bootstrap should succeed");
            // Drop is implicit when the channel sender goes out of scope
            // (it's moved into bootstrap()), so the receiver is now closed.

            assert_eq!(result.event_count, 4, "all four records should be reported");

            let events = collect_events(rx).await;
            assert_eq!(events.len(), 4, "channel should carry one event per record");

            // Each event must be a SourceChange::Insert with the expected element id.
            let mut seen_ids: Vec<String> = events
                .iter()
                .map(|e| match &e.change {
                    SourceChange::Insert { element } => match element {
                        Element::Node { metadata, .. } => metadata.reference.element_id.to_string(),
                        _ => panic!("expected Node element"),
                    },
                    other => panic!("expected Insert, got {other:?}"),
                })
                .collect();
            seen_ids.sort();
            assert_eq!(
                seen_ids,
                vec![
                    "account:id-1",
                    "account:id-2",
                    "account:id-3",
                    "account:id-4"
                ]
            );

            // Sequence numbers must be unique and monotonic.
            let mut seqs: Vec<u64> = events.iter().map(|e| e.sequence).collect();
            seqs.sort();
            assert_eq!(seqs, vec![0, 1, 2, 3]);
        }

        #[tokio::test]
        async fn filters_entities_by_requested_labels() {
            let server = MockServer::start().await;

            // Only `accounts` is mounted. If the provider mistakenly fetches
            // `contacts`, wiremock's default 404 would surface as an error.
            Mock::given(method("GET"))
                .and(path("/api/data/v9.2/accounts"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": [
                        { "accountid": "id-1", "name": "Only" }
                    ]
                })))
                .mount(&server)
                .await;

            let provider = DataverseBootstrapProvider::builder()
                .with_environment_url(server.uri())
                .with_entities(vec!["account".to_string(), "contact".to_string()])
                .with_identity_provider(StaticToken)
                .build()
                .expect("provider should build");

            let (tx, rx) = mpsc::channel(8);
            let ctx =
                BootstrapContext::new_minimal("test-server".to_string(), "test-source".to_string());
            // Only request `account` — `contact` should be skipped entirely.
            let request = make_request("q-1", vec!["account".to_string()]);

            let result = provider
                .bootstrap(request, &ctx, tx, None)
                .await
                .expect("bootstrap should succeed");
            assert_eq!(result.event_count, 1);

            let events = collect_events(rx).await;
            assert_eq!(events.len(), 1);
            match &events[0].change {
                SourceChange::Insert { element } => match element {
                    Element::Node { metadata, .. } => {
                        assert_eq!(metadata.reference.element_id.as_ref(), "account:id-1");
                    }
                    _ => panic!("expected Node"),
                },
                other => panic!("expected Insert, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn returns_zero_when_no_labels_match() {
            let server = MockServer::start().await;
            // No mocks are mounted — if the provider hits the server it gets 404.

            let provider = DataverseBootstrapProvider::builder()
                .with_environment_url(server.uri())
                .with_entities(vec!["account".to_string()])
                .with_identity_provider(StaticToken)
                .build()
                .expect("provider should build");

            let (tx, rx) = mpsc::channel(8);
            let ctx =
                BootstrapContext::new_minimal("test-server".to_string(), "test-source".to_string());
            // Request a label that the provider isn't configured for.
            let request = make_request("q-1", vec!["unrelated".to_string()]);

            let result = provider
                .bootstrap(request, &ctx, tx, None)
                .await
                .expect("bootstrap should succeed without HTTP calls");
            assert_eq!(result.event_count, 0);

            let events = collect_events(rx).await;
            assert!(events.is_empty(), "no events should be emitted");
        }
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "dataverse-bootstrap",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::DataverseBootstrapDescriptor],
);
