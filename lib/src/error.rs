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

//! Error types for drasi-lib operations.
//!
//! This module provides structured error types using `thiserror` for idiomatic Rust error handling.
//! The pattern follows major Rust libraries like `tokio`, `reqwest`, and `sqlx`:
//! - Public API returns `crate::error::Result<T>` with structured `DrasiError` variants
//! - Internal code uses `anyhow::Result<T>` for flexibility
//! - Error chains are preserved via `#[error(transparent)]` for debugging
//!
//! # Example
//!
//! ```ignore
//! use drasi_lib::error::{DrasiError, Result};
//!
//! fn example() -> Result<()> {
//!     // Pattern match on specific error variants
//!     match some_operation() {
//!         Err(DrasiError::ComponentNotFound { component_type, component_id }) => {
//!             println!("{} '{}' not found", component_type, component_id);
//!         }
//!         Err(DrasiError::InvalidState { message }) => {
//!             println!("Invalid state: {}", message);
//!         }
//!         Err(e) => return Err(e),
//!         Ok(v) => { /* ... */ }
//!     }
//!     Ok(())
//! }
//! ```

use thiserror::Error;

/// Main error type for drasi-lib operations.
///
/// This enum provides structured error variants that enable type-safe pattern matching
/// by callers. Each variant contains contextual information about the error.
#[derive(Error, Debug)]
pub enum DrasiError {
    /// Component (source, query, or reaction) was not found.
    #[error("{component_type} '{component_id}' not found")]
    ComponentNotFound {
        /// The type of component (e.g., "source", "query", "reaction")
        component_type: String,
        /// The ID of the component that was not found
        component_id: String,
    },

    /// Component already exists with the given ID.
    #[error("{component_type} '{component_id}' already exists")]
    AlreadyExists {
        /// The type of component
        component_type: String,
        /// The ID that already exists
        component_id: String,
    },

    /// Invalid configuration provided.
    #[error("Invalid configuration: {message}")]
    InvalidConfig {
        /// Description of the configuration error
        message: String,
    },

    /// Operation is not valid in the current state.
    #[error("Invalid state: {message}")]
    InvalidState {
        /// Description of the state error
        message: String,
    },

    /// Validation failed (e.g., builder validation, input validation).
    #[error("Validation failed: {message}")]
    Validation {
        /// Description of the validation error
        message: String,
    },

    /// A component operation (start, stop, delete, etc.) failed.
    #[error("Failed to {operation} {component_type} '{component_id}': {reason}")]
    OperationFailed {
        /// The type of component
        component_type: String,
        /// The ID of the component
        component_id: String,
        /// The operation that failed (e.g., "start", "stop", "delete")
        operation: String,
        /// The reason for the failure
        reason: String,
    },

    /// Internal error - wraps underlying errors while preserving the error chain.
    /// Use `.source()` to access the underlying error chain.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

// ============================================================================
// Constructor helpers for common error patterns
// ============================================================================

impl DrasiError {
    /// Create a component not found error.
    ///
    /// # Example
    /// ```ignore
    /// DrasiError::component_not_found("source", "my-source-id")
    /// ```
    pub fn component_not_found(component_type: impl Into<String>, component_id: impl Into<String>) -> Self {
        DrasiError::ComponentNotFound {
            component_type: component_type.into(),
            component_id: component_id.into(),
        }
    }

    /// Create an already exists error.
    ///
    /// # Example
    /// ```ignore
    /// DrasiError::already_exists("query", "my-query-id")
    /// ```
    pub fn already_exists(component_type: impl Into<String>, component_id: impl Into<String>) -> Self {
        DrasiError::AlreadyExists {
            component_type: component_type.into(),
            component_id: component_id.into(),
        }
    }

    /// Create an invalid configuration error.
    ///
    /// # Example
    /// ```ignore
    /// DrasiError::invalid_config("Missing required field 'query'")
    /// ```
    pub fn invalid_config(message: impl Into<String>) -> Self {
        DrasiError::InvalidConfig {
            message: message.into(),
        }
    }

    /// Create an invalid state error.
    ///
    /// # Example
    /// ```ignore
    /// DrasiError::invalid_state("Server must be initialized before starting")
    /// ```
    pub fn invalid_state(message: impl Into<String>) -> Self {
        DrasiError::InvalidState {
            message: message.into(),
        }
    }

    /// Create a validation error.
    ///
    /// # Example
    /// ```ignore
    /// DrasiError::validation("Query string cannot be empty")
    /// ```
    pub fn validation(message: impl Into<String>) -> Self {
        DrasiError::Validation {
            message: message.into(),
        }
    }

    /// Create an operation failed error.
    ///
    /// # Example
    /// ```ignore
    /// DrasiError::operation_failed("source", "my-source", "start", "Connection refused")
    /// ```
    pub fn operation_failed(
        component_type: impl Into<String>,
        component_id: impl Into<String>,
        operation: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        DrasiError::OperationFailed {
            component_type: component_type.into(),
            component_id: component_id.into(),
            operation: operation.into(),
            reason: reason.into(),
        }
    }

    // ========================================================================
    // Backward compatibility helpers (deprecated, use structured variants)
    // ========================================================================

    /// Create a provisioning error.
    ///
    /// # Deprecated
    /// Consider using `operation_failed` with specific component context instead.
    pub fn provisioning(msg: impl Into<String>) -> Self {
        DrasiError::InvalidConfig {
            message: format!("Provisioning error: {}", msg.into()),
        }
    }

    /// Create a component error (generic operation failure).
    ///
    /// # Deprecated
    /// Consider using `operation_failed` with specific component context instead.
    pub fn component_error(msg: impl Into<String>) -> Self {
        DrasiError::InvalidState {
            message: format!("Component error: {}", msg.into()),
        }
    }

    /// Create a startup validation error.
    ///
    /// # Deprecated
    /// Consider using `validation` or `invalid_config` instead.
    pub fn startup_validation(msg: impl Into<String>) -> Self {
        DrasiError::Validation {
            message: format!("Startup validation failed: {}", msg.into()),
        }
    }
}

/// Result type for drasi-lib operations.
///
/// This is the standard result type for all public API methods in drasi-lib.
/// It uses `DrasiError` which supports pattern matching on specific error variants.
pub type Result<T> = std::result::Result<T, DrasiError>;

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_component_not_found_display() {
        let err = DrasiError::component_not_found("source", "my-source");
        assert_eq!(err.to_string(), "source 'my-source' not found");
    }

    #[test]
    fn test_already_exists_display() {
        let err = DrasiError::already_exists("query", "my-query");
        assert_eq!(err.to_string(), "query 'my-query' already exists");
    }

    #[test]
    fn test_invalid_config_display() {
        let err = DrasiError::invalid_config("Missing field");
        assert_eq!(err.to_string(), "Invalid configuration: Missing field");
    }

    #[test]
    fn test_invalid_state_display() {
        let err = DrasiError::invalid_state("Not initialized");
        assert_eq!(err.to_string(), "Invalid state: Not initialized");
    }

    #[test]
    fn test_validation_display() {
        let err = DrasiError::validation("Empty query string");
        assert_eq!(err.to_string(), "Validation failed: Empty query string");
    }

    #[test]
    fn test_operation_failed_display() {
        let err = DrasiError::operation_failed("source", "my-source", "start", "Connection refused");
        assert_eq!(
            err.to_string(),
            "Failed to start source 'my-source': Connection refused"
        );
    }

    #[test]
    fn test_internal_error_from_anyhow() {
        let anyhow_err = anyhow::anyhow!("Something went wrong");
        let drasi_err: DrasiError = anyhow_err.into();
        assert!(matches!(drasi_err, DrasiError::Internal(_)));
        assert!(drasi_err.to_string().contains("Something went wrong"));
    }

    #[test]
    fn test_error_pattern_matching() {
        let err = DrasiError::component_not_found("source", "test-source");

        match err {
            DrasiError::ComponentNotFound {
                component_type,
                component_id,
            } => {
                assert_eq!(component_type, "source");
                assert_eq!(component_id, "test-source");
            }
            _ => panic!("Expected ComponentNotFound variant"),
        }
    }

    #[test]
    fn test_internal_error_transparent() {
        // Create an anyhow error with a source chain
        let io_error = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let anyhow_err = anyhow::Error::new(io_error).context("Failed to read config");
        let drasi_err: DrasiError = anyhow_err.into();

        // The error should be Internal variant
        assert!(matches!(drasi_err, DrasiError::Internal(_)));

        // The display should show the full chain due to #[error(transparent)]
        let display = drasi_err.to_string();
        assert!(display.contains("Failed to read config"));

        // source() returns the underlying anyhow error's source
        // Note: anyhow wraps errors, so source behavior depends on the chain
        if let DrasiError::Internal(ref anyhow_err) = drasi_err {
            // We can access the anyhow error and its chain
            assert!(anyhow_err.to_string().contains("Failed to read config"));
        }
    }

    #[test]
    fn test_backward_compat_provisioning() {
        let err = DrasiError::provisioning("Failed to provision");
        assert!(err.to_string().contains("Provisioning error"));
    }

    #[test]
    fn test_backward_compat_component_error() {
        let err = DrasiError::component_error("Component failed");
        assert!(err.to_string().contains("Component error"));
    }

    #[test]
    fn test_backward_compat_startup_validation() {
        let err = DrasiError::startup_validation("Validation failed");
        assert!(err.to_string().contains("Startup validation failed"));
    }
}
