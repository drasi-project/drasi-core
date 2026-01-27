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

use super::{Credentials, IdentityProvider};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use aws_config::SdkConfig;
use std::sync::Arc;

/// Identity provider for AWS IAM database authentication.
///
/// Uses AWS RDS IAM authentication to generate temporary database passwords.
/// This works with Amazon RDS and Aurora databases that have IAM authentication enabled.
#[derive(Clone)]
pub struct AwsIdentityProvider {
    config: Arc<SdkConfig>,
    username: String,
    hostname: String,
    port: u16,
    region: String,
}

impl AwsIdentityProvider {
    /// Create a new AWS IAM identity provider.
    ///
    /// Loads AWS configuration from the environment (credentials, region, etc.).
    /// This automatically uses:
    /// - Environment variables (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, etc.)
    /// - Instance profile credentials (when running on EC2/ECS)
    /// - Shared credentials file (~/.aws/credentials)
    /// - IAM roles for service accounts (when running on EKS)
    ///
    /// # Arguments
    /// * `username` - IAM database username
    /// * `hostname` - RDS endpoint hostname
    /// * `port` - Database port (typically 5432 for PostgreSQL, 3306 for MySQL)
    pub async fn new(
        username: impl Into<String>,
        hostname: impl Into<String>,
        port: u16,
    ) -> Result<Self> {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let region = config
            .region()
            .ok_or_else(|| anyhow!("AWS region not configured"))?
            .to_string();

        Ok(Self {
            config: Arc::new(config),
            username: username.into(),
            hostname: hostname.into(),
            port,
            region,
        })
    }

    /// Create a provider with an explicit region.
    ///
    /// This is useful when you want to override the region from the AWS config.
    pub async fn with_region(
        username: impl Into<String>,
        hostname: impl Into<String>,
        port: u16,
        region: impl Into<String>,
    ) -> Result<Self> {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;

        Ok(Self {
            config: Arc::new(config),
            username: username.into(),
            hostname: hostname.into(),
            port,
            region: region.into(),
        })
    }

    /// Create a provider with a custom AWS SDK configuration.
    ///
    /// This allows full control over AWS credential providers, retry policies, etc.
    pub fn with_config(
        config: SdkConfig,
        username: impl Into<String>,
        hostname: impl Into<String>,
        port: u16,
        region: impl Into<String>,
    ) -> Self {
        Self {
            config: Arc::new(config),
            username: username.into(),
            hostname: hostname.into(),
            port,
            region: region.into(),
        }
    }
}

#[async_trait]
impl IdentityProvider for AwsIdentityProvider {
    async fn get_credentials(&self) -> Result<Credentials> {
        use aws_sdk_rds::auth_token::{AuthTokenGenerator, Config};

        // Create the auth token generator configuration
        let auth_config = Config::builder()
            .hostname(&self.hostname)
            .port(self.port as u64)
            .username(&self.username)
            .build()
            .map_err(|e| anyhow!("Failed to build auth token config: {}", e))?;

        // Create the generator and generate the token
        let generator = AuthTokenGenerator::new(auth_config);
        let token = generator
            .auth_token(&self.config)
            .await
            .map_err(|e| anyhow!("Failed to generate AWS IAM token: {}", e))?;

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

    #[tokio::test]
    async fn test_with_region_creates_provider() {
        // This test validates the API without requiring actual AWS credentials
        let result = AwsIdentityProvider::with_region(
            "mydbuser",
            "mydb.rds.amazonaws.com",
            5432,
            "us-west-2",
        )
        .await;

        // The provider should be created successfully even without credentials
        // (credentials are only needed when get_credentials() is called)
        assert!(result.is_ok());
        let provider = result.unwrap();
        assert_eq!(provider.username, "mydbuser");
        assert_eq!(provider.region, "us-west-2");
        assert_eq!(provider.hostname, "mydb.rds.amazonaws.com");
        assert_eq!(provider.port, 5432);
    }

    #[test]
    fn test_with_config_creates_provider() {
        let config = SdkConfig::builder().build();
        let provider = AwsIdentityProvider::with_config(
            config,
            "testuser",
            "testdb.rds.amazonaws.com",
            3306,
            "eu-west-1",
        );

        assert_eq!(provider.username, "testuser");
        assert_eq!(provider.region, "eu-west-1");
        assert_eq!(provider.hostname, "testdb.rds.amazonaws.com");
        assert_eq!(provider.port, 3306);
    }

    #[test]
    fn test_provider_is_cloneable() {
        let config = SdkConfig::builder().build();
        let provider = AwsIdentityProvider::with_config(
            config,
            "user",
            "db.amazonaws.com",
            5432,
            "us-east-1",
        );

        let cloned = provider.clone();
        assert_eq!(cloned.username, provider.username);
        assert_eq!(cloned.region, provider.region);
        assert_eq!(cloned.hostname, provider.hostname);
        assert_eq!(cloned.port, provider.port);
    }

    #[test]
    fn test_provider_as_trait_object() {
        let config = SdkConfig::builder().build();
        let provider: Box<dyn IdentityProvider> = Box::new(AwsIdentityProvider::with_config(
            config,
            "user",
            "db.amazonaws.com",
            5432,
            "us-east-1",
        ));

        let _cloned = provider.clone();
    }
}

