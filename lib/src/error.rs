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
//! The pattern follows major Rust libraries like `tokio`, `reqwest`, and `sqlx`.
//!
//! # Error Handling Architecture
//!
//! drasi-lib uses a three-layer error strategy:
//!
//! | Layer | Error Type | When to Use |
//! |-------|-----------|-------------|
//! | **Public API** | `crate::error::Result<T>` / `DrasiError` | Methods on `DrasiLib`, `*_ops` modules, `InspectionAPI` |
//! | **Internal modules** | `anyhow::Result<T>` | Lifecycle, managers, component_graph — use `.context()` for rich chains |
//! | **Plugin traits** | `anyhow::Result<T>` | `Source`, `Reaction`, `BootstrapProvider` trait methods |
//!
//! ## Bridge: Internal → Public
//!
//! `DrasiError::Internal(#[from] anyhow::Error)` auto-converts internal `anyhow` errors
//! at the public API boundary via the `?` operator. For errors with known semantics, use
//! the structured variants directly (e.g., `DrasiError::invalid_state()`).
//! When translating an existing failure, attach it with [`DrasiError::with_cause`]
//! rather than discarding it after formatting. Such errors use the existing
//! `Internal` carrier; match [`DrasiError::classification`] for the public category
//! and use [`DrasiError::downcast_ref`] for the retained typed cause.
//!
//! ## Rules
//!
//! - **Public API methods** (on `DrasiLib`, the `*_ops` modules, `InspectionAPI`) must return
//!   `crate::error::Result<T>` with `DrasiError` variants
//! - **Internal modules** should use `anyhow::Result` with `.context("what failed")`
//! - **Plugin trait implementations** should use `anyhow::Result` with `.context()`
//! - **Never** use `anyhow!()` in public API methods — use `DrasiError` constructors
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
    /// Attach an underlying failure without changing this error's public category.
    ///
    /// The returned variant is `Internal`, since the other existing variants have
    /// no source field. Use [`Self::classification`] rather than matching the outer
    /// variant when handling errors returned by library operations.
    pub fn with_cause(self, cause: impl Into<anyhow::Error>) -> Self {
        Self::Internal(cause.into().context(self))
    }

    /// Return the outermost structured public category, including through contexts.
    /// An unclassified internal error returns itself.
    pub fn classification(&self) -> &Self {
        match self {
            Self::Internal(error) => error
                .downcast_ref::<Self>()
                .map_or(self, Self::classification),
            _ => self,
        }
    }

    /// Find a retained typed cause, including `anyhow` contexts and nested sources.
    ///
    /// Errors can contain several failures (for example `GraphError::Cleanup`).
    /// Downcast to that aggregate to inspect all failures; the standard source
    /// chain alone represents only its primary failure.
    pub fn downcast_ref<E: std::error::Error + Send + Sync + 'static>(&self) -> Option<&E> {
        match self {
            Self::Internal(error) => error
                .downcast_ref::<E>()
                .or_else(|| {
                    error
                        .downcast_ref::<Self>()
                        .and_then(Self::downcast_ref::<E>)
                })
                .or_else(|| {
                    error.chain().find_map(|cause| {
                        cause.downcast_ref::<E>().or_else(|| {
                            cause
                                .downcast_ref::<Self>()
                                .and_then(Self::downcast_ref::<E>)
                        })
                    })
                }),
            _ => None,
        }
    }

    /// Create a component not found error.
    ///
    /// # Example
    /// ```ignore
    /// DrasiError::component_not_found("source", "my-source-id")
    /// ```
    pub fn component_not_found(
        component_type: impl Into<String>,
        component_id: impl Into<String>,
    ) -> Self {
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
    pub fn already_exists(
        component_type: impl Into<String>,
        component_id: impl Into<String>,
    ) -> Self {
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
}

/// Result type for drasi-lib operations.
///
/// This is the standard result type for all public API methods in drasi-lib.
/// It uses `DrasiError` which supports pattern matching on specific error variants.
pub type Result<T> = std::result::Result<T, DrasiError>;

/// Multiple operation failures, all retained in encounter order.
///
/// The standard source chain follows the first failure. Inspect [`Self::failures`]
/// to recover other causes or ownership-bearing cleanup errors.
#[derive(Debug)]
pub struct OperationFailures {
    message: String,
    failures: Vec<anyhow::Error>,
}

impl OperationFailures {
    /// Retains the individual causes of an operation and its required cleanup.
    pub fn new(message: impl Into<String>, failures: Vec<anyhow::Error>) -> Self {
        Self {
            message: message.into(),
            failures,
        }
    }

    pub fn failures(&self) -> &[anyhow::Error] {
        &self.failures
    }
}

impl std::fmt::Display for OperationFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        for (index, error) in self.failures.iter().enumerate() {
            write!(f, "{}{error:#}", if index == 0 { ": " } else { "; " })?;
        }
        Ok(())
    }
}

impl std::error::Error for OperationFailures {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.failures.first().map(|error| error.as_ref())
    }
}

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
        let err =
            DrasiError::operation_failed("source", "my-source", "start", "Connection refused");
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
    fn public_classification_and_typed_cause_survive_contexts() {
        let cause = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "checkpoint denied",
        ))
        .context("saving checkpoint");
        let error = DrasiError::operation_failed("source", "input", "stop", cause.to_string())
            .with_cause(cause);
        let error = DrasiError::from(anyhow::Error::new(error).context("outer operation"));
        assert!(matches!(
            error.classification(),
            DrasiError::OperationFailed { component_type, component_id, operation, reason }
                if component_type == "source" && component_id == "input"
                    && operation == "stop" && reason == "saving checkpoint"
        ));
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn outer_public_classification_wins_over_inner_classification() {
        let inner = DrasiError::invalid_state("not ready");
        let error = DrasiError::operation_failed("query", "q", "start", inner.to_string())
            .with_cause(inner);
        assert!(matches!(
            error.classification(),
            DrasiError::OperationFailed { .. }
        ));
    }

    #[test]
    fn unclassified_internal_error_stays_internal() {
        let error = DrasiError::from(anyhow::anyhow!("unclassified"));
        assert!(std::ptr::eq(error.classification(), &error));
        assert!(error.downcast_ref::<std::io::Error>().is_none());
    }

    #[test]
    fn nested_internal_carriers_retain_the_root_error_not_only_its_source() {
        use crate::computation::v1::{ComponentId, GraphError};

        let graph_error = GraphError::Component {
            component: ComponentId::try_new("input").unwrap(),
            operation: "stop",
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").into(),
        };
        let inner = DrasiError::from(anyhow::Error::new(graph_error));
        let error = DrasiError::from(anyhow::Error::new(inner).context("outer operation"));
        assert!(matches!(
            error.downcast_ref::<GraphError>(),
            Some(GraphError::Component { .. })
        ));
        assert!(error.downcast_ref::<std::io::Error>().is_some());
    }

    #[test]
    fn aggregate_retains_all_causes_and_primary_downcasting() {
        use crate::context::workers::WorkerCleanupError;

        let failures = OperationFailures::new(
            "shutdown failed",
            vec![
                WorkerCleanupError::TimedOut {
                    timeout: std::time::Duration::from_secs(2),
                }
                .into(),
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").into(),
            ],
        );
        let error = DrasiError::operation_failed("source", "input", "stop", failures.to_string())
            .with_cause(failures);
        assert!(error.downcast_ref::<WorkerCleanupError>().is_some());
        let failures = error
            .downcast_ref::<OperationFailures>()
            .unwrap()
            .failures();
        assert_eq!(failures.len(), 2);
        assert_eq!(
            failures[1].downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
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
}
