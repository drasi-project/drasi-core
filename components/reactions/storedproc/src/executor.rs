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
use mysql_async::prelude::*;
use mysql_async::{Pool, OptsBuilder, SslOpts};
use tiberius::{Client as TiberiusClient, Config as TiberiusConfig, Query, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::{TokioAsyncWriteCompatExt, Compat};

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
        let port = config.get_port();

        // Build connection string
        let ssl_mode = if config.ssl { "require" } else { "disable" };
        let connection_string = format!(
            "host={} port={} user={} password={} dbname={} sslmode={}",
            config.hostname, port, config.user, config.password, config.database, ssl_mode
        );

        info!(
            "Connecting to PostgreSQL: {}:{}/{}",
            config.hostname, port, config.database
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
            config.hostname, port, config.database
        );

        Ok(Self {
            client: Arc::new(RwLock::new(client)),
            command_timeout: Duration::from_millis(config.command_timeout_ms),
            retry_attempts: config.retry_attempts,
        })
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

/// MySQL stored procedure executor
pub struct MySqlExecutor {
    pool: Pool,
    command_timeout: Duration,
    retry_attempts: u32,
}

impl MySqlExecutor {
    /// Create a new MySQL executor
    pub async fn new(config: &StoredProcReactionConfig) -> Result<Self> {
        let port = config.get_port();

        info!(
            "Connecting to MySQL: {}:{}/{}",
            config.hostname, port, config.database
        );

        // Build MySQL connection options
        let mut opts_builder = OptsBuilder::default()
            .ip_or_hostname(&config.hostname)
            .tcp_port(port)
            .user(Some(&config.user))
            .pass(Some(&config.password))
            .db_name(Some(&config.database));

        // Configure SSL if enabled
        if config.ssl {
            opts_builder = opts_builder.ssl_opts(Some(SslOpts::default()));
        }

        // Create connection pool
        let pool = Pool::new(opts_builder);

        // Test the connection
        let mut conn = pool
            .get_conn()
            .await
            .map_err(|e| anyhow!("Failed to connect to MySQL: {}", e))?;

        // Test query
        conn.query_drop("SELECT 1")
            .await
            .map_err(|e| anyhow!("Failed to test MySQL connection: {}", e))?;

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

#[async_trait]
impl DatabaseExecutor for MySqlExecutor {
    async fn test_connection(&self) -> Result<()> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| anyhow!("Failed to get connection: {}", e))?;

        timeout(self.command_timeout, conn.query_drop("SELECT 1"))
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
        let pool = self.pool.clone();
        let cmd_timeout = self.command_timeout;

        self.execute_with_retry(|| async {
            let mut conn = pool
                .get_conn()
                .await
                .map_err(|e| anyhow!("Failed to get connection: {}", e))?;

            // Build the CALL statement for MySQL
            // MySQL uses ? for parameter placeholders
            let param_placeholders: Vec<&str> = (0..params.len()).map(|_| "?").collect();

            let query = if param_placeholders.is_empty() {
                format!("CALL {}()", proc_name)
            } else {
                format!("CALL {}({})", proc_name, param_placeholders.join(", "))
            };

            debug!("Executing: {} with {} parameters", query, params.len());

            // Convert JSON values to MySQL values
            let mysql_params: Vec<mysql_async::Value> = params
                .iter()
                .map(|v| self.json_to_mysql_value(v))
                .collect();

            // Execute the stored procedure
            timeout(cmd_timeout, conn.exec_drop(&query, mysql_params))
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

    fn database_type(&self) -> &str {
        "MySQL"
    }
}

/// MS SQL Server stored procedure executor
pub struct MsSqlExecutor {
    client: Arc<RwLock<TiberiusClient<Compat<TcpStream>>>>,
    command_timeout: Duration,
    retry_attempts: u32,
}

impl MsSqlExecutor {
    /// Create a new MS SQL executor
    pub async fn new(config: &StoredProcReactionConfig) -> Result<Self> {
        let port = config.get_port();

        info!(
            "Connecting to MS SQL Server: {}:{}/{}",
            config.hostname, port, config.database
        );

        // Build MS SQL connection config
        let mut tiberius_config = TiberiusConfig::new();
        tiberius_config.host(&config.hostname);
        tiberius_config.port(port);
        tiberius_config.authentication(tiberius::AuthMethod::sql_server(
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
impl DatabaseExecutor for MsSqlExecutor {
    async fn test_connection(&self) -> Result<()> {
        let mut client = self.client.write().await;

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

    fn database_type(&self) -> &str {
        "MS SQL Server"
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
            Ok(Arc::new(MySqlExecutor::new(config).await?))
        }
        crate::config::DatabaseClient::MsSQL => {
            Ok(Arc::new(MsSqlExecutor::new(config).await?))
        }
    }
}
