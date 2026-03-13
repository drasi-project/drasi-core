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

//! Azure identity provider plugin for Drasi.
//!
//! Provides Azure AD authentication (managed identity, workload identity,
//! developer tools) as a pluggable identity provider for sources and reactions.

mod provider;

pub use provider::AzureIdentityProvider;

use async_trait::async_trait;
use drasi_lib::identity::IdentityProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

/// Authentication method for the Azure identity provider.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AzureAuthMethod {
    /// System-assigned managed identity.
    ManagedIdentity,
    /// User-assigned managed identity (requires `client_id`).
    ManagedIdentityUserAssigned,
    /// Workload identity for AKS.
    WorkloadIdentity,
    /// Developer tools credential chain (e.g., `az login`).
    DeveloperTools,
}

impl Default for AzureAuthMethod {
    fn default() -> Self {
        Self::ManagedIdentity
    }
}

/// Configuration DTO for the Azure identity provider plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AzureIdentityProviderConfigDto {
    /// The identity name used for authentication (e.g., `user@tenant.onmicrosoft.com`).
    pub identity_name: String,

    /// Authentication method to use.
    #[serde(default)]
    pub auth_method: AzureAuthMethod,

    /// Client ID for user-assigned managed identity (only used when
    /// `auth_method` is `managed_identity_user_assigned`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,

    /// Custom scope for token acquisition. Defaults to the Azure OSSRDBMS scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(utoipa::OpenApi)]
#[openapi(components(schemas(AzureIdentityProviderConfigDto, AzureAuthMethod)))]
struct AzureIdentityProviderSchemas;

/// Descriptor for the Azure identity provider plugin.
pub struct AzureIdentityProviderDescriptor;

#[async_trait]
impl IdentityProviderPluginDescriptor for AzureIdentityProviderDescriptor {
    fn kind(&self) -> &str {
        "azure"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_json(&self) -> String {
        let api = AzureIdentityProviderSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    fn config_schema_name(&self) -> &str {
        "identity.azure.AzureIdentityProviderConfig"
    }

    async fn create_identity_provider(
        &self,
        config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn IdentityProvider>> {
        let dto: AzureIdentityProviderConfigDto = serde_json::from_value(config_json.clone())?;

        let mut provider = match dto.auth_method {
            AzureAuthMethod::ManagedIdentity => AzureIdentityProvider::new(&dto.identity_name)?,
            AzureAuthMethod::ManagedIdentityUserAssigned => {
                let client_id = dto.client_id.ok_or_else(|| {
                    anyhow::anyhow!(
                        "client_id is required for managed_identity_user_assigned auth method"
                    )
                })?;
                AzureIdentityProvider::with_managed_identity(&dto.identity_name, client_id)?
            }
            AzureAuthMethod::WorkloadIdentity => {
                AzureIdentityProvider::with_workload_identity(&dto.identity_name)?
            }
            AzureAuthMethod::DeveloperTools => {
                AzureIdentityProvider::with_default_credentials(&dto.identity_name)?
            }
        };

        if let Some(scope) = dto.scope {
            provider = provider.with_scope(scope);
        }

        Ok(Box::new(provider))
    }
}

// Dynamic plugin entry point
drasi_plugin_sdk::export_plugin!(
    plugin_id = "identity-azure",
    core_version = "0.4.0",
    lib_version = "0.4.0",
    plugin_version = "0.1.0",
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
    identity_provider_descriptors = [AzureIdentityProviderDescriptor],
);
