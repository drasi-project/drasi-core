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

//! Error types for Drasi Server Core API

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
