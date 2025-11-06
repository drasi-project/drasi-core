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

//! Error types for the Drasi Server-Core library
//!
//! # Error Handling Strategy
//!
//! Server-Core uses a two-tier error handling approach:
//! - **Internal**: Modules use `anyhow::Result` for implementation flexibility
//! - **Public API**: All public functions return `Result<T, DrasiError>`
//!
//! ## Creating Errors
//!
//! Use helper methods for common scenarios:
//! ```
//! # use drasi_server_core::api::DrasiError;
//! // Component not found
//! let err = DrasiError::component_not_found("source", "my-source");
//!
//! // Configuration error
//! let err = DrasiError::configuration("Missing required field: database");
//!
//! // Database connection error
//! let err = DrasiError::database_connection("Failed to connect to localhost:5432");
//!
//! // Network timeout
//! let err = DrasiError::network_timeout("HTTP request");
//!
//! // Bootstrap error
//! let err = DrasiError::bootstrap_incompatible_config("PostgreSQL provider requires PostgreSQL source");
//!
//! // Query error
//! let err = DrasiError::query_parse_error("MATCH (n)", "Unexpected token");
//! ```
//!
//! ## Error Categories
//!
//! - **Configuration**: Invalid or missing configuration
//! - **Initialization**: Server failed to initialize
//! - **ComponentNotFound**: Component lookup failed
//! - **InvalidState**: Operation not allowed in current state
//! - **ComponentError**: Error from source, query, or reaction
//! - **Validation**: Configuration validation failed
//! - **Database**: PostgreSQL operations
//! - **Network**: Network communication failures
//! - **Redis**: Redis operations
//! - **Http**: HTTP client operations
//! - **Grpc**: gRPC operations
//! - **Bootstrap**: Bootstrap provider errors
//! - **Query**: Query compilation/execution errors
//! - **Timeout**: Operation timeouts
//! - **Io**: File system or network I/O errors
//! - **Serialization**: YAML/JSON parsing errors
//! - **Internal**: Unexpected internal errors
//!
//! ## Error Conversion
//!
//! External error types are automatically converted via the `From` trait:
//! ```
//! # use drasi_server_core::api::DrasiError;
//! // Connection errors can be created directly
//! # fn example() -> Result<(), DrasiError> {
//! let err = DrasiError::database_connection("Connection refused");
//! let result: Result<(), DrasiError> = Err(err);
//! #     Ok(())
//! # }
//! ```
//!
//! Supported auto-conversions:
//! - `tokio_postgres::Error` → `DrasiError::Database`
//! - `redis::RedisError` → `DrasiError::Redis`
//! - `reqwest::Error` → `DrasiError::Http`
//! - `tonic::Status` → `DrasiError::Grpc`
//! - `std::io::Error` → `DrasiError::Io`
//! - `serde_json::Error` → `DrasiError::Serialization`
//! - `serde_yaml::Error` → `DrasiError::Serialization`
//! - `anyhow::Error` → `DrasiError::Internal`

use thiserror::Error;

/// Result type for Drasi Server Core API operations
pub type Result<T> = std::result::Result<T, DrasiError>;

/// Error types for Drasi Server Core operations
#[derive(Error, Debug)]
pub enum DrasiError {
    /// Configuration error - invalid or missing configuration
    #[error("Configuration error: {0}")]
    Configuration(String),

    /// Initialization error - server failed to initialize
    #[error("Initialization error: {0}")]
    Initialization(String),

    /// Startup validation error - configuration failed validation during startup
    #[error("Startup validation error: {0}")]
    StartupValidation(String),

    /// Provisioning error - runtime add/remove/start operations failed
    #[error("Provisioning error: {0}")]
    Provisioning(String),

    /// Component not found - requested source, query, or reaction doesn't exist
    #[error("Component not found: {kind} '{id}'")]
    ComponentNotFound { kind: String, id: String },

    /// Invalid state - operation not allowed in current state
    #[error("Invalid state: {0}")]
    InvalidState(String),

    /// Component error - error from source, query, or reaction
    #[error("Component error ({kind} '{id}'): {message}")]
    ComponentError {
        kind: String,
        id: String,
        message: String,
    },

    /// I/O error - file system or network errors
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization error - YAML/JSON parsing errors
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// Duplicate component - component with same ID already exists
    #[error("Duplicate component: {kind} '{id}' already exists")]
    DuplicateComponent { kind: String, id: String },

    /// Validation error - configuration validation failed
    #[error("Validation error: {0}")]
    Validation(String),

    /// Internal error - unexpected internal error
    #[error("Internal error: {0}")]
    Internal(String),

    /// Database error - PostgreSQL operations with source error
    #[error("Database error: {operation}")]
    Database {
        operation: String,
        #[source]
        source: tokio_postgres::Error,
    },

    /// Database connection error - connection failures without underlying error
    #[error("Database connection error: {details}")]
    DatabaseConnection { details: String },

    /// Network error - network communication errors
    #[error("Network error: {message}")]
    Network {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Redis error - Redis operations
    #[error("Redis error: {operation}")]
    Redis {
        operation: String,
        #[source]
        source: redis::RedisError,
    },

    /// gRPC error - gRPC operations
    #[error("gRPC error: {message}")]
    Grpc {
        message: String,
        status: Option<String>,
    },

    /// HTTP error - HTTP client operations
    #[error("HTTP error: {message}")]
    Http {
        message: String,
        #[source]
        source: Option<reqwest::Error>,
    },

    /// Bootstrap error - bootstrap provider errors
    #[error("Bootstrap error: {message}")]
    Bootstrap { message: String },

    /// Query error - query compilation/execution errors
    #[error("Query error: {message}")]
    Query { message: String },

    /// Timeout error - operation timed out
    #[error("Operation timed out: {operation}")]
    Timeout { operation: String },
}

impl DrasiError {
    /// Create a configuration error
    pub fn configuration(msg: impl Into<String>) -> Self {
        Self::Configuration(msg.into())
    }

    /// Create an initialization error
    pub fn initialization(msg: impl Into<String>) -> Self {
        Self::Initialization(msg.into())
    }

    /// Create a startup validation error
    pub fn startup_validation(msg: impl Into<String>) -> Self {
        Self::StartupValidation(msg.into())
    }

    /// Create a provisioning error
    pub fn provisioning(msg: impl Into<String>) -> Self {
        Self::Provisioning(msg.into())
    }

    /// Create a component not found error
    pub fn component_not_found(kind: impl Into<String>, id: impl Into<String>) -> Self {
        Self::ComponentNotFound {
            kind: kind.into(),
            id: id.into(),
        }
    }

    /// Create an invalid state error
    pub fn invalid_state(msg: impl Into<String>) -> Self {
        Self::InvalidState(msg.into())
    }

    /// Create a component error
    pub fn component_error(
        kind: impl Into<String>,
        id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::ComponentError {
            kind: kind.into(),
            id: id.into(),
            message: message.into(),
        }
    }

    /// Create a serialization error
    pub fn serialization(msg: impl Into<String>) -> Self {
        Self::Serialization(msg.into())
    }

    /// Create a duplicate component error
    pub fn duplicate_component(kind: impl Into<String>, id: impl Into<String>) -> Self {
        Self::DuplicateComponent {
            kind: kind.into(),
            id: id.into(),
        }
    }

    /// Create a validation error
    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation(msg.into())
    }

    /// Create an internal error
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    /// Create a database connection error
    pub fn database_connection(details: impl Into<String>) -> Self {
        Self::DatabaseConnection {
            details: details.into(),
        }
    }

    /// Create a database query error
    pub fn database_query(query: impl Into<String>, err: tokio_postgres::Error) -> Self {
        Self::Database {
            operation: format!("query: {}", query.into()),
            source: err,
        }
    }

    /// Create a network timeout error
    pub fn network_timeout(operation: impl Into<String>) -> Self {
        Self::Timeout {
            operation: operation.into(),
        }
    }

    /// Create a network connection failed error
    pub fn network_connection_failed(host: impl Into<String>) -> Self {
        Self::Network {
            message: format!("Failed to connect to {}", host.into()),
            source: None,
        }
    }

    /// Create a bootstrap provider not found error
    pub fn bootstrap_provider_not_found(provider: impl Into<String>) -> Self {
        Self::Bootstrap {
            message: format!("Bootstrap provider not found: {}", provider.into()),
        }
    }

    /// Create a bootstrap incompatible config error
    pub fn bootstrap_incompatible_config(reason: impl Into<String>) -> Self {
        Self::Bootstrap {
            message: format!("Incompatible bootstrap configuration: {}", reason.into()),
        }
    }

    /// Create a query parse error
    pub fn query_parse_error(query: impl Into<String>, error: impl Into<String>) -> Self {
        Self::Query {
            message: format!("Failed to parse query '{}': {}", query.into(), error.into()),
        }
    }

    /// Create a query compilation error
    pub fn query_compilation_error(details: impl Into<String>) -> Self {
        Self::Query {
            message: format!("Query compilation failed: {}", details.into()),
        }
    }
}

// Convert from anyhow::Error to DrasiError for internal error boundaries
impl From<anyhow::Error> for DrasiError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err.to_string())
    }
}

// Convert from serde_yaml::Error
impl From<serde_yaml::Error> for DrasiError {
    fn from(err: serde_yaml::Error) -> Self {
        Self::Serialization(err.to_string())
    }
}

// Convert from serde_json::Error
impl From<serde_json::Error> for DrasiError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(err.to_string())
    }
}

// Convert from tokio_postgres::Error
impl From<tokio_postgres::Error> for DrasiError {
    fn from(err: tokio_postgres::Error) -> Self {
        Self::Database {
            operation: "database operation".to_string(),
            source: err,
        }
    }
}

// Convert from redis::RedisError
impl From<redis::RedisError> for DrasiError {
    fn from(err: redis::RedisError) -> Self {
        Self::Redis {
            operation: "redis operation".to_string(),
            source: err,
        }
    }
}

// Convert from reqwest::Error
impl From<reqwest::Error> for DrasiError {
    fn from(err: reqwest::Error) -> Self {
        Self::Http {
            message: err.to_string(),
            source: Some(err),
        }
    }
}

// Convert from tonic::Status
impl From<tonic::Status> for DrasiError {
    fn from(status: tonic::Status) -> Self {
        Self::Grpc {
            message: status.message().to_string(),
            status: Some(status.code().to_string()),
        }
    }
}
