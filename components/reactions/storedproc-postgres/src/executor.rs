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
use drasi_lib::identity::Credentials;
use log::{debug, info};
use postgres_native_tls::MakeTlsConnector;
use serde_json::Value;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tokio_postgres::types::{to_sql_checked, IsNull, ToSql, Type};
use tokio_postgres::{Client, NoTls};

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
            SqlParam::Text(s) => {
                // Bind verbatim for text targets; coerce only when the target
                // column is genuinely numeric so string-typed row data can still
                // feed numeric stored-procedure parameters. A non-numeric string
                // aimed at a numeric column is a real type error and surfaces as
                // a parse error here rather than silently corrupting the value.
                match *ty {
                    Type::INT2 => s.parse::<i16>()?.to_sql(ty, out),
                    Type::INT4 => s.parse::<i32>()?.to_sql(ty, out),
                    Type::INT8 => s.parse::<i64>()?.to_sql(ty, out),
                    Type::FLOAT4 => s.parse::<f32>()?.to_sql(ty, out),
                    Type::FLOAT8 => s.parse::<f64>()?.to_sql(ty, out),
                    _ => s.to_sql(ty, out),
                }
            }
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
    ///
    /// The `identity_provider` parameter allows injecting a credential provider
    /// from the runtime context. If provided, it takes precedence over the
    /// config's identity_provider. Falls back to config's user/password if neither is set.
    pub async fn new(
        config: &PostgresStoredProcReactionConfig,
        identity_provider: Option<Arc<dyn drasi_lib::identity::IdentityProvider>>,
    ) -> Result<Self> {
        let port = config.get_port();

        // Resolve credentials: injected provider > config provider > user/password
        let effective_provider = identity_provider.as_ref().map(|p| p.as_ref());
        let config_provider = config.identity_provider.as_deref();
        let provider = effective_provider.or(config_provider);

        let credentials = if let Some(provider) = provider {
            debug!("Using identity provider for authentication");
            let context = drasi_lib::identity::CredentialContext::new()
                .with_property("hostname", &config.hostname)
                .with_property("port", port.to_string());
            Some(provider.get_credentials(&context).await?)
        } else {
            None
        };

        let is_cert_auth = credentials.as_ref().is_some_and(|c| c.is_certificate());

        // For username/password and token auth, extract the auth pair
        let (username, password) = if let Some(creds) = &credentials {
            if !creds.is_certificate() {
                creds
                    .clone()
                    .try_into_auth_pair()
                    .map_err(|_| anyhow!("Unexpected credential type"))?
            } else {
                // Certificate auth: username is optional, password is not used
                let (_, _, cert_username) = creds
                    .clone()
                    .try_into_certificate()
                    .map_err(|_| anyhow!("Expected certificate credentials"))?;
                (cert_username.unwrap_or_default(), String::new())
            }
        } else {
            debug!("Using username/password for authentication");
            (config.user.clone(), config.password.clone())
        };

        // Build connection string
        let ssl_mode = if config.ssl || is_cert_auth {
            "require"
        } else {
            "disable"
        };

        // Log connection attempt (without password)
        debug!(
            "Connection details - host: {}, port: {}, user: {}, database: {}, ssl: {}, cert_auth: {}",
            config.hostname, port, username, config.database, ssl_mode, is_cert_auth
        );

        let connection_string = format!(
            "host={} port={} user={} password={} dbname={} sslmode={}",
            config.hostname, port, username, password, config.database, ssl_mode
        );

        info!(
            "Connecting to PostgreSQL: {}:{}/{} (SSL: {}, cert_auth: {})",
            config.hostname,
            port,
            config.database,
            config.ssl || is_cert_auth,
            is_cert_auth
        );

        // Connect to database with appropriate TLS configuration
        let client = if is_cert_auth {
            // Client certificate authentication (mTLS)
            let (cert_pem, key_pem, _) = credentials
                .expect("credentials must exist when is_cert_auth is true")
                .try_into_certificate()
                .map_err(|_| anyhow!("Expected certificate credentials"))?;

            let identity = native_tls::Identity::from_pkcs8(
                cert_pem.as_bytes(),
                key_pem.as_bytes(),
            )
            .map_err(|e| anyhow!("Failed to load client certificate: {e}. Ensure cert_pem and key_pem are valid PEM-encoded data."))?;

            let tls_connector = native_tls::TlsConnector::builder()
                .identity(identity)
                .danger_accept_invalid_hostnames(false)
                .danger_accept_invalid_certs(false)
                .build()
                .map_err(|e| {
                    anyhow!("Failed to create TLS connector with client certificate: {e}")
                })?;
            let connector = MakeTlsConnector::new(tls_connector);

            debug!("Attempting mTLS connection to PostgreSQL with client certificate...");
            let (client, connection) = tokio_postgres::connect(&connection_string, connector)
                .await
                .map_err(|e| {
                    log::error!("mTLS connection error: {e:?}");
                    anyhow!("Failed to connect to database with client certificate: {e}")
                })?;

            // Spawn connection handler
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    log::error!("PostgreSQL connection error: {e}");
                }
            });

            client
        } else if config.ssl {
            // Server-only TLS (no client certificate)
            let tls_connector = native_tls::TlsConnector::builder()
                .danger_accept_invalid_hostnames(false)
                .danger_accept_invalid_certs(false)
                .build()
                .map_err(|e| anyhow!("Failed to create TLS connector: {e}"))?;
            let connector = MakeTlsConnector::new(tls_connector);

            debug!("Attempting SSL connection to PostgreSQL with system trust store...");
            let (client, connection) = tokio_postgres::connect(&connection_string, connector)
                .await
                .map_err(|e| {
                    log::error!("SSL connection error: {e:?}");
                    anyhow!("Failed to connect to database with SSL: {e}. Ensure SSL CA certificates are installed in system trust store.")
                })?;

            // Spawn connection handler
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    log::error!("PostgreSQL connection error: {e}");
                }
            });

            client
        } else {
            let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
                .await
                .map_err(|e| anyhow!("Failed to connect to database: {e}"))?;

            // Spawn connection handler
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    log::error!("PostgreSQL connection error: {e}");
                }
            });

            client
        };

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

    /// Execute a rendered stored-procedure command with the given positional
    /// bind parameters.
    ///
    /// The `command` is the fully rendered SQL string containing `$1..$N`
    /// placeholders (produced by the Handlebars `param` helper); `parameters`
    /// are the ordered values to bind. Values are bound by `tokio-postgres`,
    /// never interpolated into the SQL text, so untrusted row data cannot alter
    /// the command structure.
    pub async fn execute_command(&self, command: &str, parameters: Vec<Value>) -> Result<()> {
        let query = command.to_string();
        let params = parameters.clone();
        let client = self.client.clone();
        let cmd_timeout = self.command_timeout;

        self.execute_with_retry(|| async {
            let client = client.read().await;

            debug!("Executing: {} with {} parameters", query, params.len());

            // Convert JSON values to SqlParam enum. Strings are kept as text
            // (preserving exact content such as leading zeros); numeric coercion,
            // when the target column is actually numeric, is handled type-aware in
            // `SqlParam::to_sql`.
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
                    Value::String(s) => SqlParam::Text(s.clone()),
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

            // Execute the rendered command
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a value via `ToSql` into a fresh buffer for comparison.
    fn encode<T: ToSql>(v: &T, ty: &Type) -> (IsNull, BytesMut) {
        let mut buf = BytesMut::new();
        let is_null = v.to_sql(ty, &mut buf).expect("encode failed");
        (is_null, buf)
    }

    #[test]
    fn text_binds_verbatim_for_text_columns() {
        // A numeric-looking string must reach a TEXT column unchanged
        // (leading zeros preserved), not coerced to a number.
        let (_, got) = encode(&SqlParam::Text("007".to_string()), &Type::TEXT);
        let (_, want) = encode(&"007", &Type::TEXT);
        assert_eq!(got, want);
        assert_eq!(&got[..], b"007");
    }

    #[test]
    fn text_coerces_to_int_for_int_columns() {
        let (_, got) = encode(&SqlParam::Text("42".to_string()), &Type::INT4);
        let (_, want) = encode(&42i32, &Type::INT4);
        assert_eq!(got, want);
    }

    #[test]
    fn text_coerces_to_float_for_float_columns() {
        let (_, got) = encode(&SqlParam::Text("1.5".to_string()), &Type::FLOAT8);
        let (_, want) = encode(&1.5f64, &Type::FLOAT8);
        assert_eq!(got, want);
    }

    #[test]
    fn text_non_numeric_into_numeric_column_errors() {
        let mut buf = BytesMut::new();
        let result = SqlParam::Text("not-a-number".to_string()).to_sql(&Type::INT4, &mut buf);
        assert!(result.is_err());
    }

    #[test]
    fn numeric_json_value_still_binds_to_text_column() {
        // Integers/floats coming from JSON numbers still stringify for TEXT columns.
        let (_, got) = encode(&SqlParam::Int(42), &Type::TEXT);
        assert_eq!(&got[..], b"42");
    }
}
