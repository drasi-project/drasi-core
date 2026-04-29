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

//! Configuration for the Dataverse bootstrap provider.

use std::collections::HashMap;

/// Configuration for the Dataverse bootstrap provider.
///
/// The bootstrap provider fetches initial data from Dataverse entities using
/// the OData Web API. It pages through all records and sends them as
/// `SourceChange::Insert` events.
///
/// # Example
///
/// ```
/// use drasi_bootstrap_dataverse::DataverseBootstrapConfig;
///
/// let config = DataverseBootstrapConfig {
///     environment_url: "https://myorg.crm.dynamics.com".to_string(),
///     tenant_id: "00000000-0000-0000-0000-000000000001".to_string(),
///     client_id: "00000000-0000-0000-0000-000000000002".to_string(),
///     client_secret: "my-secret".to_string(),
///     use_azure_cli: false,
///     entities: vec!["account".to_string(), "contact".to_string()],
///     entity_set_overrides: Default::default(),
///     entity_columns: Default::default(),
///     api_version: "v9.2".to_string(),
///     page_size: 5000,
/// };
/// assert!(config.validate().is_ok());
/// ```
#[derive(Debug, Clone)]
pub struct DataverseBootstrapConfig {
    /// Dataverse environment URL (e.g., "https://myorg.crm.dynamics.com")
    pub environment_url: String,

    /// Azure AD tenant ID for authentication.
    /// Required for client credentials flow, ignored when `use_azure_cli` is true.
    pub tenant_id: String,

    /// Azure AD application (client) ID.
    /// Required for client credentials flow, ignored when `use_azure_cli` is true.
    pub client_id: String,

    /// Azure AD client secret.
    /// Required for client credentials flow, ignored when `use_azure_cli` is true.
    pub client_secret: String,

    /// Use Azure CLI (`az account get-access-token`) for authentication.
    /// When true, `tenant_id`, `client_id`, and `client_secret` are not required.
    pub use_azure_cli: bool,

    /// Entity logical names to bootstrap (e.g., ["account", "contact"])
    pub entities: Vec<String>,

    /// Override entity set names for non-standard pluralization
    /// Key: entity logical name, Value: entity set name
    pub entity_set_overrides: HashMap<String, String>,

    /// Per-entity column selection for `$select`
    /// Key: entity logical name, Value: list of column names
    pub entity_columns: HashMap<String, Vec<String>>,

    /// Dataverse Web API version (default: "v9.2")
    pub api_version: String,

    /// Number of records per page (default: 5000)
    pub page_size: usize,
}

fn default_api_version() -> String {
    "v9.2".to_string()
}

fn default_page_size() -> usize {
    5000
}

impl Default for DataverseBootstrapConfig {
    fn default() -> Self {
        Self {
            environment_url: String::new(),
            tenant_id: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            use_azure_cli: false,
            entities: Vec::new(),
            entity_set_overrides: HashMap::new(),
            entity_columns: HashMap::new(),
            api_version: default_api_version(),
            page_size: default_page_size(),
        }
    }
}

impl DataverseBootstrapConfig {
    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), String> {
        if self.environment_url.is_empty() {
            return Err("environment_url is required".to_string());
        }
        if !self.use_azure_cli {
            if self.tenant_id.is_empty() {
                return Err("tenant_id is required (or set use_azure_cli = true)".to_string());
            }
            if self.client_id.is_empty() {
                return Err("client_id is required (or set use_azure_cli = true)".to_string());
            }
            if self.client_secret.is_empty() {
                return Err("client_secret is required (or set use_azure_cli = true)".to_string());
            }
        }
        if self.entities.is_empty() {
            return Err("at least one entity is required".to_string());
        }
        Ok(())
    }

    /// Validate config when an external identity provider handles authentication.
    ///
    /// Only checks `environment_url` and `entities`; credential fields are not
    /// required because the identity provider supplies tokens directly.
    pub fn validate_with_identity_provider(&self) -> Result<(), String> {
        if self.environment_url.is_empty() {
            return Err("environment_url is required".to_string());
        }
        if self.entities.is_empty() {
            return Err("at least one entity is required".to_string());
        }
        Ok(())
    }

    /// Get the entity set name for a given entity logical name.
    ///
    /// Checks overrides first, then falls back to appending 's'.
    pub fn entity_set_name(&self, entity: &str) -> String {
        if let Some(override_name) = self.entity_set_overrides.get(entity) {
            override_name.clone()
        } else {
            format!("{entity}s")
        }
    }

    /// Get the `$select` columns for a specific entity.
    ///
    /// Ensures that the primary key column (`{entity}id`) is always included
    /// when a column list is configured, so bootstrap records always have an ID.
    pub fn select_columns(&self, entity: &str) -> Option<String> {
        self.entity_columns
            .get(entity)
            .filter(|cols| !cols.is_empty())
            .map(|cols| {
                let primary_key = format!("{entity}id");
                let has_primary_key = cols.iter().any(|c| c.eq_ignore_ascii_case(&primary_key));
                if has_primary_key {
                    cols.join(",")
                } else {
                    let mut all_columns = cols.clone();
                    all_columns.push(primary_key);
                    all_columns.join(",")
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_values() {
        let config = DataverseBootstrapConfig::default();
        assert_eq!(config.api_version, "v9.2");
        assert_eq!(config.page_size, 5000);
        assert!(config.entities.is_empty());
    }

    #[test]
    fn test_validate_success() {
        let config = DataverseBootstrapConfig {
            environment_url: "https://test.crm.dynamics.com".to_string(),
            tenant_id: "t".to_string(),
            client_id: "c".to_string(),
            client_secret: "s".to_string(),
            entities: vec!["account".to_string()],
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_missing_url() {
        let config = DataverseBootstrapConfig {
            tenant_id: "t".to_string(),
            client_id: "c".to_string(),
            client_secret: "s".to_string(),
            entities: vec!["account".to_string()],
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_empty_entities() {
        let config = DataverseBootstrapConfig {
            environment_url: "https://test.crm.dynamics.com".to_string(),
            tenant_id: "t".to_string(),
            client_id: "c".to_string(),
            client_secret: "s".to_string(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_entity_set_name() {
        let config = DataverseBootstrapConfig {
            environment_url: "https://test.crm.dynamics.com".to_string(),
            tenant_id: "t".to_string(),
            client_id: "c".to_string(),
            client_secret: "s".to_string(),
            entities: vec!["account".to_string()],
            ..Default::default()
        };
        assert_eq!(config.entity_set_name("account"), "accounts");
    }

    #[test]
    fn test_entity_set_name_with_override() {
        let mut config = DataverseBootstrapConfig {
            environment_url: "https://test.crm.dynamics.com".to_string(),
            tenant_id: "t".to_string(),
            client_id: "c".to_string(),
            client_secret: "s".to_string(),
            entities: vec!["activityparty".to_string()],
            ..Default::default()
        };
        config
            .entity_set_overrides
            .insert("activityparty".to_string(), "activityparties".to_string());
        assert_eq!(config.entity_set_name("activityparty"), "activityparties");
    }

    #[test]
    fn test_select_columns() {
        let mut config = DataverseBootstrapConfig::default();
        config.entity_columns.insert(
            "account".to_string(),
            vec!["name".to_string(), "revenue".to_string()],
        );
        // Primary key "accountid" is auto-appended
        assert_eq!(
            config.select_columns("account"),
            Some("name,revenue,accountid".to_string())
        );
        assert_eq!(config.select_columns("contact"), None);
    }

    #[test]
    fn test_select_columns_appends_primary_key() {
        let mut config = DataverseBootstrapConfig::default();
        config
            .entity_columns
            .insert("account".to_string(), vec!["name".to_string()]);
        assert_eq!(
            config.select_columns("account"),
            Some("name,accountid".to_string())
        );
    }

    #[test]
    fn test_select_columns_does_not_duplicate_primary_key() {
        let mut config = DataverseBootstrapConfig::default();
        config.entity_columns.insert(
            "account".to_string(),
            vec!["name".to_string(), "accountid".to_string()],
        );
        assert_eq!(
            config.select_columns("account"),
            Some("name,accountid".to_string())
        );
    }

    #[test]
    fn test_config_validation_azure_cli_no_secret_needed() {
        let config = DataverseBootstrapConfig {
            environment_url: "https://test.crm.dynamics.com".to_string(),
            use_azure_cli: true,
            entities: vec!["account".to_string()],
            ..Default::default()
        };
        assert!(
            config.validate().is_ok(),
            "Azure CLI mode should not require client credentials"
        );
    }
}
