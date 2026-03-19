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

//! AWS identity provider plugin for Drasi.
//!
//! Provides AWS IAM authentication as a pluggable identity provider
//! for sources and reactions. When used with RDS/Aurora databases,
//! the caller provides endpoint details (hostname, port) via
//! [`CredentialContext`](drasi_lib::identity::CredentialContext).

mod provider;

pub use provider::AwsIdentityProvider;

use async_trait::async_trait;
use drasi_lib::identity::IdentityProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

/// Configuration DTO for the AWS identity provider plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AwsIdentityProviderConfigDto {
    /// IAM database username.
    pub username: String,

    /// AWS region (e.g., `"us-west-2"`). If omitted, loaded from environment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// IAM role ARN to assume (e.g., `"arn:aws:iam::123456789012:role/RDSAccessRole"`).
    /// If provided, the provider will assume this role before generating tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role_arn: Option<String>,

    /// STS session name when assuming a role. Defaults to `"drasi-rds-session"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
}

#[derive(utoipa::OpenApi)]
#[openapi(components(schemas(AwsIdentityProviderConfigDto)))]
struct AwsIdentityProviderSchemas;

/// Descriptor for the AWS identity provider plugin.
pub struct AwsIdentityProviderDescriptor;

#[async_trait]
impl IdentityProviderPluginDescriptor for AwsIdentityProviderDescriptor {
    fn kind(&self) -> &str {
        "aws"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_json(&self) -> String {
        let api = AwsIdentityProviderSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    fn config_schema_name(&self) -> &str {
        "identity.aws.AwsIdentityProviderConfig"
    }

    async fn create_identity_provider(
        &self,
        config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn IdentityProvider>> {
        let dto: AwsIdentityProviderConfigDto = serde_json::from_value(config_json.clone())?;

        let provider = if let Some(role_arn) = dto.role_arn {
            AwsIdentityProvider::with_assumed_role(dto.username, role_arn, dto.session_name).await?
        } else if let Some(region) = dto.region {
            AwsIdentityProvider::with_region(dto.username, region).await?
        } else {
            AwsIdentityProvider::new(dto.username).await?
        };

        Ok(Box::new(provider))
    }
}

// Dynamic plugin entry point
drasi_plugin_sdk::export_plugin!(
    plugin_id = "identity-aws",
    core_version = "0.4.0",
    lib_version = "0.4.0",
    plugin_version = "0.1.0",
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
    identity_provider_descriptors = [AwsIdentityProviderDescriptor],
);
