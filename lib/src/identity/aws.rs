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

    #[test]
    fn test_multiple_providers_different_regions() {
        let config = SdkConfig::builder().build();

        let provider1 = AwsIdentityProvider::with_config(
            config.clone(),
            "user1",
            "db1.rds.amazonaws.com",
            5432,
            "us-west-2",
        );

        let provider2 = AwsIdentityProvider::with_config(
            config,
            "user2",
            "db2.rds.amazonaws.com",
            3306,
            "eu-west-1",
        );

        assert_eq!(provider1.username, "user1");
        assert_eq!(provider1.region, "us-west-2");
        assert_eq!(provider1.hostname, "db1.rds.amazonaws.com");
        assert_eq!(provider1.port, 5432);

        assert_eq!(provider2.username, "user2");
        assert_eq!(provider2.region, "eu-west-1");
        assert_eq!(provider2.hostname, "db2.rds.amazonaws.com");
        assert_eq!(provider2.port, 3306);
    }

    #[test]
    fn test_different_database_ports() {
        let config = SdkConfig::builder().build();

        // PostgreSQL
        let pg_provider = AwsIdentityProvider::with_config(
            config.clone(),
            "pguser",
            "pg.rds.amazonaws.com",
            5432,
            "us-east-1",
        );
        assert_eq!(pg_provider.port, 5432);

        // MySQL
        let mysql_provider = AwsIdentityProvider::with_config(
            config.clone(),
            "mysqluser",
            "mysql.rds.amazonaws.com",
            3306,
            "us-east-1",
        );
        assert_eq!(mysql_provider.port, 3306);

        // Aurora PostgreSQL
        let aurora_provider = AwsIdentityProvider::with_config(
            config,
            "aurorauser",
            "aurora-cluster.cluster-xxx.us-east-1.rds.amazonaws.com",
            5432,
            "us-east-1",
        );
        assert_eq!(aurora_provider.port, 5432);
    }

    #[test]
    fn test_valid_rds_hostnames() {
        let config = SdkConfig::builder().build();

        let hostnames = vec![
            "mydb.123456789012.us-east-1.rds.amazonaws.com",
            "aurora-cluster.cluster-abc123.eu-west-1.rds.amazonaws.com",
            "mydb-instance-1.abc123def456.ap-southeast-2.rds.amazonaws.com",
        ];

        for hostname in hostnames {
            let provider = AwsIdentityProvider::with_config(
                config.clone(),
                "testuser",
                hostname,
                5432,
                "us-east-1",
            );
            assert_eq!(provider.hostname, hostname);
        }
    }

    #[test]
    fn test_valid_aws_regions() {
        let config = SdkConfig::builder().build();

        let regions = vec![
            "us-east-1",
            "us-west-2",
            "eu-west-1",
            "eu-central-1",
            "ap-southeast-1",
            "ap-northeast-1",
            "sa-east-1",
            "ca-central-1",
        ];

        for region in regions {
            let provider = AwsIdentityProvider::with_config(
                config.clone(),
                "testuser",
                "test.rds.amazonaws.com",
                5432,
                region,
            );
            assert_eq!(provider.region, region);
        }
    }

    #[test]
    fn test_provider_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AwsIdentityProvider>();
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    /// Integration test that requires actual AWS credentials and RDS instance
    /// Run with: cargo test --features aws-identity,integration-tests -- --ignored
    /// Requires: AWS credentials configured (env vars, ~/.aws/credentials, or instance profile)
    #[tokio::test]
    #[ignore] // Ignored by default, run explicitly with --ignored
    async fn test_aws_iam_authentication_real() {
        // NOTE: Update these values to match your actual RDS instance
        let username = "iamuser";
        let hostname = "mydb.123456789012.us-east-1.rds.amazonaws.com";
        let port = 5432;
        let region = "us-east-1";

        let provider = AwsIdentityProvider::with_region(username, hostname, port, region)
            .await
            .expect("Failed to create AWS provider. Make sure AWS credentials are configured.");

        let credentials = provider
            .get_credentials()
            .await
            .expect("Failed to get credentials. Make sure AWS credentials are valid and RDS instance exists.");

        match credentials {
            Credentials::Token { username: user, token } => {
                assert_eq!(user, username);
                assert!(!token.is_empty());
                assert!(token.contains(&hostname)); // Token should contain the hostname
                println!("✓ Successfully generated AWS IAM auth token");
                println!("  Token length: {}", token.len());
                println!("  Token is a signed URL for RDS authentication");
            }
            _ => panic!("Expected Token credentials"),
        }
    }

    /// Test token generation for Aurora cluster
    #[tokio::test]
    #[ignore]
    async fn test_aurora_iam_authentication_real() {
        let username = "iamuser";
        let hostname = "aurora-cluster.cluster-xxx.us-east-1.rds.amazonaws.com";
        let port = 5432;

        let provider = AwsIdentityProvider::new(username, hostname, port)
            .await
            .expect("Failed to create AWS provider");

        let result = provider.get_credentials().await;

        match result {
            Ok(Credentials::Token { token, .. }) => {
                assert!(!token.is_empty());
                println!("✓ Successfully generated Aurora IAM auth token");
            }
            Ok(_) => panic!("Expected Token credentials"),
            Err(e) => {
                println!("⚠ Failed to generate token (expected if Aurora cluster doesn't exist)");
                println!("  Error: {}", e);
            }
        }
    }
}

