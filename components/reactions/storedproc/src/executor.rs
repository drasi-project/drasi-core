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

//! Database executors for stored procedure invocation.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use log::{debug, info};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, NoTls};

use crate::config::StoredProcReactionConfig;

/// Trait for database-specific stored procedure execution
#[async_trait]
pub trait DatabaseExecutor: Send + Sync {
    /// Test the database connection
    async fn test_connection(&self) -> Result<()>;

    /// Execute a stored procedure with the given parameters
    async fn execute_procedure(&self, procedure_name: &str, parameters: Vec<Value>)
        -> Result<()>;

    /// Get the database type name
    fn database_type(&self) -> &str;
}

/// PostgreSQL stored procedure executor
pub struct PostgresExecutor {
    client: Arc<RwLock<Client>>,
    command_timeout: Duration,
    retry_attempts: u32,
}

impl PostgresExecutor {
    /// Create a new PostgreSQL executor
    pub async fn new(config: &StoredProcReactionConfig) -> Result<Self> {
        // Build connection string
        let ssl_mode = if config.ssl { "require" } else { "disable" };
        let connection_string = format!(
            "host={} port={} user={} password={} dbname={} sslmode={}",
            config.hostname, config.port, config.user, config.password, config.database, ssl_mode
        );

        info!(
            "Connecting to PostgreSQL: {}:{}/{}",
            config.hostname, config.port, config.database
        );

        // Connect to database
        let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
            .await
            .map_err(|e| anyhow!("Failed to connect to database: {}", e))?;

        // Spawn connection handler
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                log::error!("PostgreSQL connection error: {}", e);
            }
        });

        info!(
            "Connected to PostgreSQL: {}:{}/{}",
            config.hostname, config.port, config.database
        );

        Ok(Self {
            client: Arc::new(RwLock::new(client)),
            command_timeout: Duration::from_millis(config.command_timeout_ms),
            retry_attempts: config.retry_attempts,
        })
    }

    /// Convert JSON value to PostgreSQL parameter
    fn json_to_sql_value(&self, value: &Value) -> String {
        match value {
            Value::Null => "NULL".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            Value::String(s) => format!("'{}'", s.replace('\'', "''")), // Escape single quotes
            Value::Array(_) | Value::Object(_) => {
                // For complex types, serialize as JSON string
                format!("'{}'", value.to_string().replace('\'', "''"))
            }
        }
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

#[async_trait]
impl DatabaseExecutor for PostgresExecutor {
    async fn test_connection(&self) -> Result<()> {
        let client = self.client.read().await;

        timeout(self.command_timeout, client.simple_query("SELECT 1"))
            .await
            .map_err(|_| anyhow!("Connection test timed out"))?
            .map_err(|e| anyhow!("Connection test failed: {}", e))?;

        info!("Database connection test successful");
        Ok(())
    }

    async fn execute_procedure(
        &self,
        procedure_name: &str,
        parameters: Vec<Value>,
    ) -> Result<()> {
        let proc_name = procedure_name.to_string();
        let params = parameters.clone();
        let client = self.client.clone();
        let cmd_timeout = self.command_timeout;

        self.execute_with_retry(|| async {
            let client = client.read().await;

            // Build the CALL statement
            // For tokio-postgres, we need to use parameterized queries with $1, $2, etc.
            let param_placeholders: Vec<String> = (1..=params.len())
                .map(|i| format!("${}", i))
                .collect();

            let query = if param_placeholders.is_empty() {
                format!("CALL {}()", proc_name)
            } else {
                format!("CALL {}({})", proc_name, param_placeholders.join(", "))
            };

            debug!("Executing: {} with {} parameters", query, params.len());

            // Convert JSON values to ToSql trait objects
            // We need to handle different types appropriately
            let sql_params: Vec<Box<dyn ToSql + Sync + Send>> = params
                .iter()
                .map(|v| match v {
                    Value::Null => Box::new(None::<String>) as Box<dyn ToSql + Sync + Send>,
                    Value::Bool(b) => Box::new(*b) as Box<dyn ToSql + Sync + Send>,
                    Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            Box::new(i) as Box<dyn ToSql + Sync + Send>
                        } else if let Some(f) = n.as_f64() {
                            Box::new(f) as Box<dyn ToSql + Sync + Send>
                        } else {
                            Box::new(n.to_string()) as Box<dyn ToSql + Sync + Send>
                        }
                    }
                    Value::String(s) => Box::new(s.clone()) as Box<dyn ToSql + Sync + Send>,
                    Value::Array(_) | Value::Object(_) => {
                        // For complex types, pass as JSON
                        Box::new(v.clone()) as Box<dyn ToSql + Sync + Send>
                    }
                })
                .collect();

            // Create references for the execute call
            let param_refs: Vec<&(dyn ToSql + Sync)> = sql_params
                .iter()
                .map(|p| p.as_ref() as &(dyn ToSql + Sync))
                .collect();

            // Execute the stored procedure
            let result = timeout(cmd_timeout, client.execute(&query, &param_refs[..]))
                .await
                .map_err(|_| {
                    anyhow!(
                        "Procedure execution timed out after {:?}",
                        cmd_timeout
                    )
                })?
                .map_err(|e| anyhow!("Failed to execute procedure: {}", e))?;

            debug!("Procedure executed successfully, rows affected: {}", result);
            Ok(())
        })
        .await
    }

    fn database_type(&self) -> &str {
        "PostgreSQL"
    }
}

/// Create a database executor based on the configuration
pub async fn create_executor(
    config: &StoredProcReactionConfig,
) -> Result<Arc<dyn DatabaseExecutor>> {
    match config.database_client {
        crate::config::DatabaseClient::PostgreSQL => {
            Ok(Arc::new(PostgresExecutor::new(config).await?))
        }
        crate::config::DatabaseClient::MySQL => {
            Err(anyhow!(
                "MySQL support not yet implemented. Use PostgreSQL for now."
            ))
        }
        crate::config::DatabaseClient::MsSQL => {
            Err(anyhow!(
                "MS SQL support not yet implemented. Use PostgreSQL for now."
            ))
        }
    }
}
