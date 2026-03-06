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

//! Configuration types for Azure Storage reactions.

use drasi_lib::reactions::common::{self, TemplateRouting};
use std::collections::HashMap;

pub use common::{OperationType, QueryConfig, TemplateSpec};

pub(crate) fn default_blob_content_type() -> String {
    "application/json".to_string()
}

/// Azure Storage target type.
#[derive(Debug, Clone, PartialEq)]
pub enum StorageTarget {
    /// Send results to Azure Blob Storage.
    Blob {
        /// Blob container name.
        container_name: String,
        /// Handlebars template for blob path/name.
        blob_path_template: String,
        /// MIME content type for blob uploads.
        content_type: String,
    },
    /// Send results as queue messages to Azure Queue Storage.
    Queue {
        /// Queue name.
        queue_name: String,
    },
    /// Store results as entities in Azure Table Storage.
    Table {
        /// Table name.
        table_name: String,
        /// Handlebars template for PartitionKey.
        partition_key_template: String,
        /// Handlebars template for RowKey.
        row_key_template: String,
    },
}

/// Configuration for Azure Storage reaction.
#[derive(Debug, Clone, PartialEq)]
pub struct AzureStorageReactionConfig {
    /// Azure storage account name.
    pub account_name: String,
    /// Azure storage account access key.
    pub access_key: String,
    /// Destination target details.
    pub target: StorageTarget,
    /// Custom blob endpoint for local emulators like Azurite.
    pub blob_endpoint: Option<String>,
    /// Custom queue endpoint for local emulators like Azurite.
    pub queue_endpoint: Option<String>,
    /// Custom table endpoint for local emulators like Azurite.
    pub table_endpoint: Option<String>,
    /// Query-specific template routes.
    pub routes: HashMap<String, QueryConfig>,
    /// Fallback template routes used when query-specific route is not found.
    pub default_template: Option<QueryConfig>,
}

impl Default for AzureStorageReactionConfig {
    fn default() -> Self {
        Self {
            account_name: String::new(),
            access_key: String::new(),
            target: StorageTarget::Queue {
                queue_name: "drasi-events".to_string(),
            },
            blob_endpoint: None,
            queue_endpoint: None,
            table_endpoint: None,
            routes: HashMap::new(),
            default_template: None,
        }
    }
}

impl TemplateRouting for AzureStorageReactionConfig {
    fn routes(&self) -> &HashMap<String, QueryConfig> {
        &self.routes
    }

    fn default_template(&self) -> Option<&QueryConfig> {
        self.default_template.as_ref()
    }
}

impl AzureStorageReactionConfig {
    /// Return a normalized copy with trimmed non-template fields.
    pub fn normalized(&self) -> Self {
        let mut normalized = self.clone();
        normalized.account_name = normalized.account_name.trim().to_string();
        normalized.access_key = normalized.access_key.trim().to_string();
        normalized.blob_endpoint = normalized
            .blob_endpoint
            .as_ref()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        normalized.queue_endpoint = normalized
            .queue_endpoint
            .as_ref()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        normalized.table_endpoint = normalized
            .table_endpoint
            .as_ref()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        normalized.target = match &normalized.target {
            StorageTarget::Blob {
                container_name,
                blob_path_template,
                content_type,
            } => StorageTarget::Blob {
                container_name: container_name.trim().to_string(),
                blob_path_template: blob_path_template.clone(),
                content_type: content_type.trim().to_string(),
            },
            StorageTarget::Queue { queue_name } => StorageTarget::Queue {
                queue_name: queue_name.trim().to_string(),
            },
            StorageTarget::Table {
                table_name,
                partition_key_template,
                row_key_template,
            } => StorageTarget::Table {
                table_name: table_name.trim().to_string(),
                partition_key_template: partition_key_template.clone(),
                row_key_template: row_key_template.clone(),
            },
        };

        normalized
    }

    /// Validate required fields.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.account_name.trim().is_empty() {
            anyhow::bail!("account_name is required");
        }
        if self.access_key.trim().is_empty() {
            anyhow::bail!("access_key is required");
        }

        match &self.target {
            StorageTarget::Blob {
                container_name,
                blob_path_template,
                content_type,
            } => {
                if container_name.trim().is_empty() {
                    anyhow::bail!("blob target requires container_name");
                }
                if blob_path_template.trim().is_empty() {
                    anyhow::bail!("blob target requires blob_path_template");
                }
                if content_type.trim().is_empty() {
                    anyhow::bail!("blob target requires non-empty content_type");
                }
            }
            StorageTarget::Queue { queue_name } => {
                if queue_name.trim().is_empty() {
                    anyhow::bail!("queue target requires queue_name");
                }
            }
            StorageTarget::Table {
                table_name,
                partition_key_template,
                row_key_template,
            } => {
                if table_name.trim().is_empty() {
                    anyhow::bail!("table target requires table_name");
                }
                if partition_key_template.trim().is_empty() {
                    anyhow::bail!("table target requires partition_key_template");
                }
                if row_key_template.trim().is_empty() {
                    anyhow::bail!("table target requires row_key_template");
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blob_target_default_content_type() {
        let target = StorageTarget::Blob {
            container_name: "my-container".to_string(),
            blob_path_template: "{{after.id}}.txt".to_string(),
            content_type: default_blob_content_type(),
        };

        match target {
            StorageTarget::Blob { content_type, .. } => {
                assert_eq!(content_type, "application/json");
            }
            _ => panic!("expected blob target"),
        }
    }

    #[test]
    fn test_config_validation() {
        let config = AzureStorageReactionConfig {
            account_name: "acct".to_string(),
            access_key: "key".to_string(),
            target: StorageTarget::Queue {
                queue_name: "events".to_string(),
            },
            ..Default::default()
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_query_routing_falls_back_to_default() {
        let config = AzureStorageReactionConfig {
            account_name: "acct".to_string(),
            access_key: "key".to_string(),
            target: StorageTarget::Queue {
                queue_name: "events".to_string(),
            },
            routes: HashMap::new(),
            default_template: Some(QueryConfig {
                added: Some(TemplateSpec::new("{{json after}}")),
                ..Default::default()
            }),
            ..Default::default()
        };

        let spec = config.get_template_spec("unknown-query", OperationType::Add);
        assert!(spec.is_some());
        assert_eq!(
            spec.expect("template should exist").template,
            "{{json after}}"
        );
    }

    #[test]
    fn test_config_normalization_trims_credentials() {
        let config = AzureStorageReactionConfig {
            account_name: "  acct  ".to_string(),
            access_key: "  key  ".to_string(),
            target: StorageTarget::Queue {
                queue_name: " events ".to_string(),
            },
            queue_endpoint: Some(" http://127.0.0.1:10001/devstoreaccount1 ".to_string()),
            ..Default::default()
        };

        let normalized = config.normalized();
        assert_eq!(normalized.account_name, "acct");
        assert_eq!(normalized.access_key, "key");
        assert_eq!(
            normalized.target,
            StorageTarget::Queue {
                queue_name: "events".to_string(),
            }
        );
        assert_eq!(
            normalized.queue_endpoint.as_deref(),
            Some("http://127.0.0.1:10001/devstoreaccount1")
        );
    }
}
