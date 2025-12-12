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

//! MS SQL Server executor for stored procedure invocation.

use anyhow::{anyhow, Result};
use log::{debug, info};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tiberius::{AuthMethod, Client as TiberiusClient, Config as TiberiusConfig, EncryptionLevel, Query};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tokio_util::compat::{TokioAsyncWriteCompatExt, Compat};

use crate::config::MsSqlStoredProcReactionConfig;

/// MS SQL Server stored procedure executor
pub struct MsSqlExecutor {
    client: Arc<RwLock<TiberiusClient<Compat<TcpStream>>>>,
    command_timeout: Duration,
    retry_attempts: u32,
}

impl MsSqlExecutor {
    /// Create a new MS SQL executor
    pub async fn new(config: &MsSqlStoredProcReactionConfig) -> Result<Self> {
        let port = config.get_port();

        info!(
            "Connecting to MS SQL Server: {}:{}/{}",
            config.hostname, port, config.database
        );

        // Build MS SQL connection config
        let mut tiberius_config = TiberiusConfig::new();
        tiberius_config.host(&config.hostname);
        tiberius_config.port(port);
        tiberius_config.authentication(AuthMethod::sql_server(
            &config.user,
            &config.password,
        ));
        tiberius_config.database(&config.database);
        tiberius_config.trust_cert(); // For self-signed certificates

        // Configure encryption
        if config.ssl {
            tiberius_config.encryption(EncryptionLevel::Required);
        } else {
            tiberius_config.encryption(EncryptionLevel::NotSupported);
        }

        // Connect to the database
        let tcp = TcpStream::connect(tiberius_config.get_addr())
            .await
            .map_err(|e| anyhow!("Failed to connect to MS SQL Server: {}", e))?;

        tcp.set_nodelay(true)
            .map_err(|e| anyhow!("Failed to set TCP_NODELAY: {}", e))?;

        let client = TiberiusClient::connect(tiberius_config, tcp.compat_write())
            .await
            .map_err(|e| anyhow!("Failed to establish MS SQL connection: {}", e))?;

        info!(
            "Connected to MS SQL Server: {}:{}/{}",
            config.hostname, port, config.database
        );

        Ok(Self {
            client: Arc::new(RwLock::new(client)),
            command_timeout: Duration::from_millis(config.command_timeout_ms),
            retry_attempts: config.retry_attempts,
        })
    }

    /// Test the database connection
    pub async fn test_connection(&self) -> Result<()> {
        let mut client = self.client.write().await;

        timeout(self.command_timeout, client.simple_query("SELECT 1"))
            .await
            .map_err(|_| anyhow!("Connection test timed out"))?
            .map_err(|e| anyhow!("Connection test failed: {}", e))?;

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
        let client = self.client.clone();
        let cmd_timeout = self.command_timeout;

        self.execute_with_retry(|| async {
            let mut client = client.write().await;

            // Build the EXEC statement for MS SQL
            // MS SQL uses @p1, @p2, @p3 for parameter placeholders
            let param_placeholders: Vec<String> = (1..=params.len())
                .map(|i| format!("@p{}", i))
                .collect();

            let query_str = if param_placeholders.is_empty() {
                format!("EXEC {}", proc_name)
            } else {
                format!("EXEC {} {}", proc_name, param_placeholders.join(", "))
            };

            debug!("Executing: {} with {} parameters", query_str, params.len());

            // Build parameterized query
            let mut query = Query::new(query_str);

            // Bind parameters - tiberius requires explicit type binding
            for param in params.iter() {
                match param {
                    Value::Null => {
                        query.bind(None::<&str>);
                    }
                    Value::Bool(b) => {
                        query.bind(*b as i32);
                    }
                    Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            // Try to fit in i32, otherwise use i64
                            if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
                                query.bind(i as i32);
                            } else {
                                query.bind(i);
                            }
                        } else if let Some(f) = n.as_f64() {
                            query.bind(f);
                        } else {
                            query.bind(n.to_string());
                        }
                    }
                    Value::String(s) => {
                        query.bind(s.as_str());
                    }
                    Value::Array(_) | Value::Object(_) => {
                        // For complex types, pass as JSON string
                        query.bind(param.to_string());
                    }
                }
            }

            // Execute the stored procedure
            timeout(cmd_timeout, query.execute(&mut *client))
                .await
                .map_err(|_| {
                    anyhow!(
                        "Procedure execution timed out after {:?}",
                        cmd_timeout
                    )
                })?
                .map_err(|e| anyhow!("Failed to execute procedure: {}", e))?;

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
                debug!("Retrying after {:?} (attempt {})", backoff, attempt);
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
}
