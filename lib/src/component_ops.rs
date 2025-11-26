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

//! Generic component operations for source, query, and reaction managers
//!
//! This module provides generic helper functions to eliminate duplication in component
//! lifecycle management. The error mapping logic is centralized here so that start/stop/delete
//! operations for sources, queries, and reactions can share the same error handling code.

use crate::error::DrasiError;
use anyhow::Result as AnyhowResult;

/// Maps anyhow::Error from manager operations to DrasiError
///
/// This centralizes the error mapping logic used by start/stop operations across
/// all component types (sources, queries, reactions).
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
/// Returns the original result on success, or maps anyhow::Error to appropriate DrasiError
pub fn map_component_error<T>(
    result: AnyhowResult<T>,
    component_type: &str,
    component_id: &str,
    operation: &str,
) -> crate::error::Result<T> {
    result.map_err(|e| {
        let error_msg = e.to_string();
        if error_msg.contains("not found") {
            DrasiError::component_not_found(component_type, component_id)
        } else {
            DrasiError::component_error(format!(
                "Failed to {} {} '{}': {}",
                operation, component_type, component_id, e
            ))
        }
    })
}

/// Maps anyhow::Error to DrasiError for state-related errors
///
/// This is used for operations where state validation is the primary concern
/// (e.g., query/reaction operations that check if dependencies are ready).
///
/// # Arguments
///
/// * `result` - The result from a manager operation
/// * `component_type` - The type of component ("source", "query", "reaction")
/// * `component_id` - The ID of the component
///
/// # Returns
///
/// Returns the original result on success, or maps anyhow::Error to appropriate DrasiError
pub fn map_state_error<T>(
    result: AnyhowResult<T>,
    component_type: &str,
    component_id: &str,
) -> crate::error::Result<T> {
    result.map_err(|e| {
        let error_msg = e.to_string();
        if error_msg.contains("not found") {
            DrasiError::component_not_found(component_type, component_id)
        } else {
            DrasiError::invalid_state(error_msg)
        }
    })
}

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
    fn test_map_component_error_not_found() {
        let result: AnyhowResult<()> = Err(anyhow!("Source not found: test-id"));
        let mapped = map_component_error(result, "source", "test-id", "start");
        assert!(mapped.is_err());
        let err = mapped.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_map_component_error_generic() {
        let result: AnyhowResult<()> = Err(anyhow!("Connection timeout"));
        let mapped = map_component_error(result, "query", "test-query", "stop");
        assert!(mapped.is_err());
        let err = mapped.unwrap_err();
        assert!(err.to_string().contains("Failed to stop query"));
    }

    #[test]
    fn test_map_state_error_success() {
        let result: AnyhowResult<String> = Ok("success".to_string());
        let mapped = map_state_error(result, "reaction", "test-reaction");
        assert!(mapped.is_ok());
        assert_eq!(mapped.unwrap(), "success");
    }

    #[test]
    fn test_map_state_error_not_found() {
        let result: AnyhowResult<()> = Err(anyhow!("Component not found"));
        let mapped = map_state_error(result, "query", "missing-query");
        assert!(mapped.is_err());
        let err = mapped.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_map_state_error_invalid_state() {
        let result: AnyhowResult<()> = Err(anyhow!("Component is already running"));
        let mapped = map_state_error(result, "source", "running-source");
        assert!(mapped.is_err());
        let err = mapped.unwrap_err();
        assert!(err.to_string().contains("already running"));
    }
}
