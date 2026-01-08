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

//! PostgreSQL executor for stored procedure invocation.

use anyhow::{anyhow, Result};
use bytes::BytesMut;
use log::{debug, info};
use serde_json::Value;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tokio_postgres::types::{to_sql_checked, IsNull, ToSql, Type};
use tokio_postgres::{Client, NoTls};

use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;

use crate::config::PostgresStoredProcReactionConfig;

/// Wrapper enum for SQL parameters that implements ToSql
#[derive(Debug, Clone)]
enum SqlParam {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Json(Value),
}

impl ToSql for SqlParam {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        match self {
            SqlParam::Null => None::<String>.to_sql(ty, out),
            SqlParam::Bool(b) => b.to_sql(ty, out),
            SqlParam::Int(i) => {
                // Try to match the target type
                match *ty {
                    Type::INT2 => (*i as i16).to_sql(ty, out),
                    Type::INT4 => (*i as i32).to_sql(ty, out),
                    Type::INT8 => i.to_sql(ty, out),
                    Type::FLOAT4 => (*i as f32).to_sql(ty, out),
                    Type::FLOAT8 => (*i as f64).to_sql(ty, out),
                    Type::NUMERIC => i.to_sql(ty, out),
                    Type::TEXT | Type::VARCHAR => i.to_string().to_sql(ty, out),
                    _ => i.to_sql(ty, out),
                }
            }
            SqlParam::Float(f) => {
                // Try to match the target type
                match *ty {
                    Type::FLOAT4 => (*f as f32).to_sql(ty, out),
                    Type::FLOAT8 => f.to_sql(ty, out),
                    Type::NUMERIC => f.to_sql(ty, out),
                    Type::TEXT | Type::VARCHAR => f.to_string().to_sql(ty, out),
                    _ => f.to_sql(ty, out),
                }
            }
            SqlParam::Text(s) => s.to_sql(ty, out),
            SqlParam::Json(v) => v.to_sql(ty, out),
        }
    }

    fn accepts(ty: &Type) -> bool {
        // Accept most common PostgreSQL types
        matches!(
            *ty,
            Type::BOOL
                | Type::INT2
                | Type::INT4
                | Type::INT8
                | Type::FLOAT4
                | Type::FLOAT8
                | Type::NUMERIC
                | Type::TEXT
                | Type::VARCHAR
                | Type::JSON
                | Type::JSONB
        ) || <String as ToSql>::accepts(ty)
            || <i64 as ToSql>::accepts(ty)
            || <f64 as ToSql>::accepts(ty)
            || <bool as ToSql>::accepts(ty)
    }

    to_sql_checked!();
}

/// PostgreSQL stored procedure executor
pub struct PostgresExecutor {
    client: Arc<RwLock<Client>>,
    command_timeout: Duration,
    retry_attempts: u32,
}

impl PostgresExecutor {
    /// Create a new PostgreSQL executor
    pub async fn new(config: &PostgresStoredProcReactionConfig) -> Result<Self> {
        let port = config.get_port();

        // Determine password - use aad_token if available, otherwise use password
        let password = if let Some(ref token) = config.aad_token {
            debug!("Using Azure AD token authentication");
            token
        } else if let Some(ref pwd) = config.password {
            debug!("Using password authentication");
            pwd
        } else {
            return Err(anyhow!("Either password or aad_token must be provided"));
        };

        // Build connection string
        let ssl_mode = if config.ssl { "require" } else { "disable" };
        let connection_string = format!(
            "host={} port={} user={} password={} dbname={} sslmode={}",
            config.hostname, port, config.user, password, config.database, ssl_mode
        );

        info!(
            "Connecting to PostgreSQL: {}:{}/{}",
            config.hostname, port, config.database
        );

        // Connect to database with appropriate TLS configuration
        if config.ssl {
            // Use native TLS for secure connections
            let tls_connector = TlsConnector::builder()
                .build()
                .map_err(|e| anyhow!("Failed to build TLS connector: {e}"))?;
            let tls = MakeTlsConnector::new(tls_connector);

            let (client, connection) = tokio_postgres::connect(&connection_string, tls)
                .await
                .map_err(|e| anyhow!("Failed to connect to database: {e}"))?;

            // Spawn connection handler
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    log::error!("PostgreSQL connection error: {e}");
                }
            });

            info!(
                "Connected to PostgreSQL: {}:{}/{} (SSL enabled)",
                config.hostname, port, config.database
            );

            Ok(Self {
                client: Arc::new(RwLock::new(client)),
                command_timeout: Duration::from_millis(config.command_timeout_ms),
                retry_attempts: config.retry_attempts,
            })
        } else {
            // Use NoTls for non-secure connections
            let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
                .await
                .map_err(|e| anyhow!("Failed to connect to database: {e}"))?;

            // Spawn connection handler
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    log::error!("PostgreSQL connection error: {e}");
                }
            });

            info!(
                "Connected to PostgreSQL: {}:{}/{} (SSL disabled)",
                config.hostname, port, config.database
            );

            Ok(Self {
                client: Arc::new(RwLock::new(client)),
                command_timeout: Duration::from_millis(config.command_timeout_ms),
                retry_attempts: config.retry_attempts,
            })
        }
    }

    /// Test the database connection
    pub async fn test_connection(&self) -> Result<()> {
        let client = self.client.read().await;

        timeout(self.command_timeout, client.simple_query("SELECT 1"))
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
        let client = self.client.clone();
        let cmd_timeout = self.command_timeout;

        self.execute_with_retry(|| async {
            let client = client.read().await;

            // Build the CALL statement
            // For tokio-postgres, we need to use parameterized queries with $1, $2, etc.
            let param_placeholders: Vec<String> =
                (1..=params.len()).map(|i| format!("${i}")).collect();

            let query = if param_placeholders.is_empty() {
                format!("CALL {proc_name}()")
            } else {
                format!("CALL {}({})", proc_name, param_placeholders.join(", "))
            };

            debug!("Executing: {} with {} parameters", query, params.len());

            // Convert JSON values to SqlParam enum
            // For numbers, try to preserve the numeric type for stored procedures with numeric parameters
            // For strings that look like numbers, try to parse them as f64 for DOUBLE PRECISION compatibility
            let sql_params: Vec<SqlParam> = params
                .iter()
                .map(|v| match v {
                    Value::Null => SqlParam::Null,
                    Value::Bool(b) => SqlParam::Bool(*b),
                    Value::Number(n) => {
                        // Keep numbers as their native type
                        if let Some(i) = n.as_i64() {
                            SqlParam::Int(i)
                        } else if let Some(f) = n.as_f64() {
                            SqlParam::Float(f)
                        } else {
                            SqlParam::Text(n.to_string())
                        }
                    }
                    Value::String(s) => {
                        // Try to parse strings as numbers for compatibility with numeric columns
                        // If it's a valid number, pass it as f64, otherwise keep as string
                        if let Ok(f) = s.parse::<f64>() {
                            SqlParam::Float(f)
                        } else {
                            SqlParam::Text(s.clone())
                        }
                    }
                    Value::Array(_) | Value::Object(_) => {
                        // For complex types, pass as JSON
                        SqlParam::Json(v.clone())
                    }
                })
                .collect();

            // Create references for the execute call
            let param_refs: Vec<&(dyn ToSql + Sync)> = sql_params
                .iter()
                .map(|p| p as &(dyn ToSql + Sync))
                .collect();

            // Execute the stored procedure
            let result = timeout(cmd_timeout, client.execute(&query, &param_refs[..]))
                .await
                .map_err(|_| anyhow!("Procedure execution timed out after {cmd_timeout:?}"))?
                .map_err(|e| anyhow!("Failed to execute procedure: {e}"))?;

            debug!("Procedure executed successfully, rows affected: {result}");
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
                // Use saturating_pow and saturating_mul to prevent overflow, and cap the backoff to 30 seconds.
                let max_backoff = Duration::from_secs(30);
                let exp = attempt - 1;
                let backoff_millis = 100u64.saturating_mul(2u64.saturating_pow(exp));
                let backoff = Duration::from_millis(backoff_millis).min(max_backoff);
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
}
