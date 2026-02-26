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

//! MySQL executor for stored procedure invocation.

use anyhow::{anyhow, Result};
use drasi_lib::identity::Credentials;
use log::{debug, info};
use mysql_async::prelude::*;
use mysql_async::{OptsBuilder, Pool, SslOpts};
use serde_json::Value;
use std::time::Duration;
use tokio::time::timeout;

use crate::config::MySqlStoredProcReactionConfig;

/// MySQL stored procedure executor
pub struct MySqlExecutor {
    pool: Pool,
    command_timeout: Duration,
    retry_attempts: u32,
}

impl MySqlExecutor {
    /// Create a new MySQL executor
    ///
    /// The `identity_provider` parameter allows injecting a credential provider
    /// from the runtime context. If provided, it takes precedence over the
    /// config's identity_provider. Falls back to config's user/password if neither is set.
    pub async fn new(
        config: &MySqlStoredProcReactionConfig,
        identity_provider: Option<std::sync::Arc<dyn drasi_lib::identity::IdentityProvider>>,
    ) -> Result<Self> {
        let port = config.get_port();

        // Resolve credentials: injected provider > config provider > user/password
        let effective_provider = identity_provider.as_ref().map(|p| p.as_ref());
        let config_provider = config.identity_provider.as_deref();
        let provider = effective_provider.or(config_provider);

        let (username, password) = if let Some(provider) = provider {
            debug!("Using identity provider for authentication");
            let credentials = provider.get_credentials().await?;
            if credentials.is_certificate() {
                anyhow::bail!(
                    "Certificate-based authentication is not supported for MySQL. \
                     The mysql_async driver requires PKCS12 format which is incompatible \
                     with PEM-based credentials. Use token or password authentication instead."
                );
            }
            credentials.into_auth_pair()
        } else {
            debug!("Using username/password for authentication");
            (config.user.clone(), config.password.clone())
        };

        info!(
            "Connecting to MySQL: {}:{}/{}",
            config.hostname, port, config.database
        );

        // Build MySQL connection options
        let mut opts_builder = OptsBuilder::default()
            .ip_or_hostname(&config.hostname)
            .tcp_port(port)
            .user(Some(&username))
            .pass(Some(&password))
            .db_name(Some(&config.database));

        // Configure SSL if enabled
        // Uses system trust store for certificate validation
        // For AWS RDS on macOS: sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain ~/rds-ca-bundle.pem
        if config.ssl {
            opts_builder = opts_builder.ssl_opts(Some(SslOpts::default()));
        }

        // Create connection pool
        let pool = Pool::new(opts_builder);

        // Test the connection
        let mut conn = pool
            .get_conn()
            .await
            .map_err(|e| anyhow!("Failed to connect to MySQL: {e}"))?;

        // Test query
        conn.query_drop("SELECT 1")
            .await
            .map_err(|e| anyhow!("Failed to test MySQL connection: {e}"))?;

        // Return connection to pool
        drop(conn);

        info!(
            "Connected to MySQL: {}:{}/{}",
            config.hostname, port, config.database
        );

        Ok(Self {
            pool,
            command_timeout: Duration::from_millis(config.command_timeout_ms),
            retry_attempts: config.retry_attempts,
        })
    }

    /// Test the database connection
    pub async fn test_connection(&self) -> Result<()> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| anyhow!("Failed to get connection: {e}"))?;

        timeout(self.command_timeout, conn.query_drop("SELECT 1"))
            .await
            .map_err(|_| anyhow!("Connection test timed out"))?
            .map_err(|e| anyhow!("Connection test failed: {e}"))?;

        info!("Database connection test successful");
        Ok(())
    }

    /// Execute a stored procedure with the given parameters
    pub async fn execute_procedure(
        &self,
        procedure_name: &str,
        parameters: Vec<Value>,
    ) -> Result<()> {
        let proc_name = procedure_name.to_string();
        let params = parameters.clone();
        let pool = self.pool.clone();
        let cmd_timeout = self.command_timeout;

        self.execute_with_retry(|| async {
            let mut conn = pool
                .get_conn()
                .await
                .map_err(|e| anyhow!("Failed to get connection: {e}"))?;

            // Build the CALL statement for MySQL
            // MySQL uses ? for parameter placeholders
            let param_placeholders: Vec<&str> = (0..params.len()).map(|_| "?").collect();

            let query = if param_placeholders.is_empty() {
                format!("CALL {proc_name}()")
            } else {
                format!("CALL {}({})", proc_name, param_placeholders.join(", "))
            };

            debug!("Executing: {} with {} parameters", query, params.len());

            let mysql_params: Vec<mysql_async::Value> =
                params.iter().map(|v| self.json_to_mysql_value(v)).collect();

            // Execute the stored procedure
            timeout(cmd_timeout, conn.exec_drop(&query, mysql_params))
                .await
                .map_err(|_| anyhow!("Procedure execution timed out after {cmd_timeout:?}"))?
                .map_err(|e| anyhow!("Failed to execute procedure: {e}"))?;

            debug!("Procedure executed successfully");
            Ok(())
        })
        .await
    }

    /// Execute with retry logic
    async fn execute_with_retry<F, Fut>(&self, operation: F) -> Result<()>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let mut last_error = None;

        for attempt in 0..=self.retry_attempts {
            if attempt > 0 {
                let backoff = Duration::from_millis(100 * 2u64.pow(attempt - 1));
                debug!("Retrying after {backoff:?} (attempt {attempt})");
                tokio::time::sleep(backoff).await;
            }

            match operation().await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_error = Some(e);
                    if attempt < self.retry_attempts {
                        debug!("Attempt {} failed, retrying...", attempt + 1);
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("Operation failed with no error")))
    }

    /// Convert JSON value to MySQL Value
    fn json_to_mysql_value(&self, value: &Value) -> mysql_async::Value {
        match value {
            Value::Null => mysql_async::Value::NULL,
            Value::Bool(b) => mysql_async::Value::Int(*b as i64),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    mysql_async::Value::Int(i)
                } else if let Some(f) = n.as_f64() {
                    mysql_async::Value::Double(f)
                } else {
                    mysql_async::Value::Bytes(n.to_string().into_bytes())
                }
            }
            Value::String(s) => mysql_async::Value::Bytes(s.as_bytes().to_vec()),
            Value::Array(_) | Value::Object(_) => {
                // For complex types, serialize as JSON string
                mysql_async::Value::Bytes(value.to_string().into_bytes())
            }
        }
    }
}
