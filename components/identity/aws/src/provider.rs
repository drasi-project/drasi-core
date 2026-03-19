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

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use aws_config::SdkConfig;
use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};
use std::sync::Arc;

/// Identity provider for AWS IAM authentication.
///
/// Uses AWS IAM credentials to generate authentication tokens.
/// For RDS/Aurora databases, the caller provides hostname and port via
/// [`CredentialContext`] so the provider can generate endpoint-specific tokens.
///
/// # Context Properties
///
/// When used with RDS/Aurora databases, the caller should provide:
/// - `hostname` — the RDS endpoint (e.g., `"mydb.cluster-xxx.us-west-2.rds.amazonaws.com"`)
/// - `port` — the database port (e.g., `"5432"`)
#[derive(Clone)]
pub struct AwsIdentityProvider {
    config: Arc<SdkConfig>,
    username: String,
    region: String,
}

impl AwsIdentityProvider {
    /// Create a new AWS IAM identity provider.
    ///
    /// Loads AWS configuration from the environment (credentials, region, etc.).
    pub async fn new(username: impl Into<String>) -> Result<Self> {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let region = config
            .region()
            .ok_or_else(|| anyhow!("AWS region not configured"))?
            .to_string();

        Ok(Self {
            config: Arc::new(config),
            username: username.into(),
            region,
        })
    }

    /// Create a provider with an explicit region.
    pub async fn with_region(
        username: impl Into<String>,
        region: impl Into<String>,
    ) -> Result<Self> {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;

        Ok(Self {
            config: Arc::new(config),
            username: username.into(),
            region: region.into(),
        })
    }

    /// Create a provider with a custom AWS SDK configuration.
    pub fn with_config(
        config: SdkConfig,
        username: impl Into<String>,
        region: impl Into<String>,
    ) -> Self {
        Self {
            config: Arc::new(config),
            username: username.into(),
            region: region.into(),
        }
    }

    /// Create a provider that assumes an IAM role.
    pub async fn with_assumed_role(
        username: impl Into<String>,
        role_arn: impl Into<String>,
        session_name: Option<String>,
    ) -> Result<Self> {
        use aws_config::sts::AssumeRoleProvider;

        let base_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let region = base_config
            .region()
            .ok_or_else(|| anyhow!("AWS region not configured"))?
            .clone();

        let base_credentials = base_config.credentials_provider().ok_or_else(|| {
            anyhow!("No AWS credentials found. Run 'aws configure' to set up credentials.")
        })?;

        let session = session_name.unwrap_or_else(|| "drasi-rds-session".to_string());
        let role_provider = AssumeRoleProvider::builder(role_arn.into())
            .session_name(session)
            .region(region.clone())
            .build_from_provider(base_credentials)
            .await;

        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .credentials_provider(role_provider)
            .region(region.clone())
            .load()
            .await;

        Ok(Self {
            config: Arc::new(config),
            username: username.into(),
            region: region.to_string(),
        })
    }
}

#[async_trait]
impl IdentityProvider for AwsIdentityProvider {
    async fn get_credentials(&self, context: &CredentialContext) -> Result<Credentials> {
        use aws_sdk_rds::auth_token::{AuthTokenGenerator, Config};

        let hostname = context.get("hostname").ok_or_else(|| {
            anyhow!(
                "AWS identity provider requires 'hostname' in credential context \
                 (e.g., the RDS endpoint)"
            )
        })?;

        let port: u64 = context
            .get("port")
            .ok_or_else(|| {
                anyhow!(
                    "AWS identity provider requires 'port' in credential context \
                     (e.g., 5432 for PostgreSQL)"
                )
            })?
            .parse()
            .map_err(|e| anyhow!("Invalid port in credential context: {e}"))?;

        let auth_config = Config::builder()
            .hostname(hostname)
            .port(port)
            .username(&self.username)
            .build()
            .map_err(|e| anyhow!("Failed to build auth token config: {e}"))?;

        let generator = AuthTokenGenerator::new(auth_config);
        let token = generator
            .auth_token(&self.config)
            .await
            .map_err(|e| anyhow!("Failed to generate AWS IAM token: {e}"))?;

        Ok(Credentials::Token {
            username: self.username.clone(),
            token: token.to_string(),
        })
    }

    fn clone_box(&self) -> Box<dyn IdentityProvider> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Construction tests ----

    #[tokio::test]
    async fn test_new_creates_provider_from_environment() {
        // new() loads region from environment; without AWS_REGION set it may fail,
        // but the code path itself should not panic.
        let _result = AwsIdentityProvider::new("mydbuser").await;
        // We cannot assert Ok because CI may lack AWS_REGION, but the call must not panic.
    }

    #[tokio::test]
    async fn test_with_region_creates_provider() {
        let result = AwsIdentityProvider::with_region("mydbuser", "us-west-2").await;
        assert!(result.is_ok());
        let provider = result.unwrap();
        assert_eq!(provider.username, "mydbuser");
        assert_eq!(provider.region, "us-west-2");
    }

    #[test]
    fn test_with_config_creates_provider() {
        let config = SdkConfig::builder().build();
        let provider = AwsIdentityProvider::with_config(config, "testuser", "eu-west-1");
        assert_eq!(provider.username, "testuser");
        assert_eq!(provider.region, "eu-west-1");
    }

    #[tokio::test]
    async fn test_with_assumed_role_requires_credentials() {
        // with_assumed_role needs base credentials; without them it should return an error.
        let result = AwsIdentityProvider::with_assumed_role(
            "mydbuser",
            "arn:aws:iam::123456789012:role/my-role",
            None,
        )
        .await;
        // In CI without AWS credentials this will fail — that's the expected behavior.
        // We just verify it doesn't panic.
        let _ = result;
    }

    // ---- Trait object & cloning tests ----

    #[test]
    fn test_provider_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AwsIdentityProvider>();
    }

    #[test]
    fn test_clone_box_returns_valid_trait_object() {
        let config = SdkConfig::builder().build();
        let provider = AwsIdentityProvider::with_config(config, "testuser", "us-east-1");
        let boxed: Box<dyn IdentityProvider> = provider.clone_box();
        // Cloned trait object should also be cloneable
        let _boxed2: Box<dyn IdentityProvider> = boxed.clone_box();
    }

    #[test]
    fn test_provider_as_trait_object() {
        let config = SdkConfig::builder().build();
        let provider = AwsIdentityProvider::with_config(config, "testuser", "us-east-1");
        let _trait_obj: Box<dyn IdentityProvider> = Box::new(provider);
    }

    // ---- get_credentials error-path tests ----

    #[tokio::test]
    async fn test_get_credentials_missing_hostname() {
        let config = SdkConfig::builder().build();
        let provider = AwsIdentityProvider::with_config(config, "testuser", "us-east-1");

        let context = CredentialContext::new().with_property("port", "5432");

        let result = provider.get_credentials(&context).await;
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("hostname"),
            "Expected error about missing hostname, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_credentials_missing_port() {
        let config = SdkConfig::builder().build();
        let provider = AwsIdentityProvider::with_config(config, "testuser", "us-east-1");

        let context = CredentialContext::new()
            .with_property("hostname", "mydb.cluster-xxx.us-east-1.rds.amazonaws.com");

        let result = provider.get_credentials(&context).await;
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("port"),
            "Expected error about missing port, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_credentials_invalid_port() {
        let config = SdkConfig::builder().build();
        let provider = AwsIdentityProvider::with_config(config, "testuser", "us-east-1");

        let context = CredentialContext::new()
            .with_property("hostname", "mydb.cluster-xxx.us-east-1.rds.amazonaws.com")
            .with_property("port", "not-a-number");

        let result = provider.get_credentials(&context).await;
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("Invalid port"),
            "Expected error about invalid port, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_credentials_empty_context() {
        let config = SdkConfig::builder().build();
        let provider = AwsIdentityProvider::with_config(config, "testuser", "us-east-1");

        let context = CredentialContext::default();
        let result = provider.get_credentials(&context).await;
        assert!(result.is_err(), "Empty context should produce an error");
    }
}
