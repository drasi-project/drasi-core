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

use std::fmt;

/// Main error type for drasi-lib operations
#[derive(Debug)]
pub enum DrasiError {
    /// Component not found
    NotFound(String),
    /// Invalid configuration
    InvalidConfig(String),
    /// Component already exists
    AlreadyExists(String),
    /// Operation failed
    OperationFailed(String),
    /// Internal error
    Internal(anyhow::Error),
}

impl DrasiError {
    /// Create a component not found error
    pub fn component_not_found(component_type: &str, component_id: &str) -> Self {
        DrasiError::NotFound(format!("{} '{}' not found", component_type, component_id))
    }

    /// Create a provisioning error
    pub fn provisioning(msg: impl Into<String>) -> Self {
        DrasiError::OperationFailed(format!("Provisioning error: {}", msg.into()))
    }

    /// Create an invalid state error
    pub fn invalid_state(msg: impl Into<String>) -> Self {
        DrasiError::OperationFailed(format!("Invalid state: {}", msg.into()))
    }

    /// Create a component error
    pub fn component_error(msg: impl Into<String>) -> Self {
        DrasiError::OperationFailed(format!("Component error: {}", msg.into()))
    }

    /// Create a startup validation error
    pub fn startup_validation(msg: impl Into<String>) -> Self {
        DrasiError::InvalidConfig(format!("Startup validation failed: {}", msg.into()))
    }
}

impl fmt::Display for DrasiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DrasiError::NotFound(msg) => write!(f, "Not found: {}", msg),
            DrasiError::InvalidConfig(msg) => write!(f, "Invalid configuration: {}", msg),
            DrasiError::AlreadyExists(msg) => write!(f, "Already exists: {}", msg),
            DrasiError::OperationFailed(msg) => write!(f, "Operation failed: {}", msg),
            DrasiError::Internal(err) => write!(f, "Internal error: {}", err),
        }
    }
}

impl std::error::Error for DrasiError {}

impl From<anyhow::Error> for DrasiError {
    fn from(err: anyhow::Error) -> Self {
        DrasiError::Internal(err)
    }
}

/// Result type for drasi-lib operations
pub type Result<T> = std::result::Result<T, DrasiError>;