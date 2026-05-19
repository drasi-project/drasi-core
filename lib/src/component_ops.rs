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

//! Generic component operations for source, query, and reaction managers.
//!
//! This module provides helper functions for converting internal `anyhow::Error` results
//! to structured `DrasiError` variants at the public API boundary.
//!
//! # Error Conversion Pattern
//!
//! Internal manager code uses `anyhow::Result<T>` for flexibility. At the public API
//! boundary (in `lib_core_ops/*.rs`), these are converted to `DrasiError` variants
//! using the helpers in this module.
//!
//! ```ignore
//! // Internal manager returns anyhow::Result
//! let result = self.source_manager.start_source(id).await;
//!
//! // Convert to DrasiError at API boundary
//! map_component_error(result, "source", id, "start")
//! ```

use crate::error::DrasiError;
use anyhow::Result as AnyhowResult;

// ============================================================================
// Error mapping functions
// ============================================================================

/// Maps `anyhow::Error` from manager operations to `DrasiError`.
///
/// This function converts internal errors to structured `DrasiError` variants
/// at the public API boundary. It uses the `OperationFailed` variant which
/// includes full context about the component and operation.
///
/// # Arguments
///
/// * `result` - The result from a manager operation
/// * `component_type` - The type of component ("source", "query", "reaction")
/// * `component_id` - The ID of the component
/// * `operation` - The operation being performed ("start", "stop", "delete")
///
/// # Returns
///
/// Returns the original value on success, or a structured `DrasiError` on failure.
///
/// # Example
///
/// ```ignore
/// let result = self.source_manager.start_source(id.to_string()).await;
/// map_component_error(result, "source", id, "start")
/// ```
pub fn map_component_error<T>(
    result: AnyhowResult<T>,
    component_type: &str,
    component_id: &str,
    operation: &str,
) -> crate::error::Result<T> {
    result.map_err(|e| {
        DrasiError::operation_failed(component_type, component_id, operation, e.to_string())
    })
}

/// Maps `anyhow::Error` to `DrasiError` for state-related errors.
///
/// This is used for operations where state validation is the primary concern
/// (e.g., checking if dependencies are ready). It uses the `InvalidState` variant.
///
/// # Arguments
///
/// * `result` - The result from a manager operation
/// * `_component_type` - The type of component (reserved for future use)
/// * `_component_id` - The ID of the component (reserved for future use)
///
/// # Returns
///
/// Returns the original value on success, or an `InvalidState` error on failure.
pub fn map_state_error<T>(
    result: AnyhowResult<T>,
    _component_type: &str,
    _component_id: &str,
) -> crate::error::Result<T> {
    result.map_err(|e| DrasiError::invalid_state(e.to_string()))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn test_map_component_error_success() {
        let result: AnyhowResult<i32> = Ok(42);
        let mapped = map_component_error(result, "source", "test-id", "start");
        assert!(mapped.is_ok());
        assert_eq!(mapped.unwrap(), 42);
    }

    #[test]
    fn test_map_component_error_failure() {
        let result: AnyhowResult<()> = Err(anyhow!("Connection timeout"));
        let mapped = map_component_error(result, "query", "test-query", "stop");
        assert!(mapped.is_err());

        let err = mapped.unwrap_err();
        match err {
            DrasiError::OperationFailed {
                component_type,
                component_id,
                operation,
                reason,
            } => {
                assert_eq!(component_type, "query");
                assert_eq!(component_id, "test-query");
                assert_eq!(operation, "stop");
                assert!(reason.contains("Connection timeout"));
            }
            _ => panic!("Expected OperationFailed variant"),
        }
    }

    #[test]
    fn test_map_state_error_success() {
        let result: AnyhowResult<String> = Ok("success".to_string());
        let mapped = map_state_error(result, "reaction", "test-reaction");
        assert!(mapped.is_ok());
        assert_eq!(mapped.unwrap(), "success");
    }

    #[test]
    fn test_map_state_error_failure() {
        let result: AnyhowResult<()> = Err(anyhow!("Component is already running"));
        let mapped = map_state_error(result, "source", "running-source");
        assert!(mapped.is_err());

        let err = mapped.unwrap_err();
        match err {
            DrasiError::InvalidState { message } => {
                assert!(message.contains("already running"));
            }
            _ => panic!("Expected InvalidState variant"),
        }
    }
}
