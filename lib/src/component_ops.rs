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
//! ```rust
//! use drasi_lib::component_ops::map_component_error;
//! use drasi_lib::error::DrasiError;
//! let cause = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
//! let result: anyhow::Result<()> = Err(cause.into());
//! let source_id = drasi_lib::ComponentId::generate();
//!
//! // Convert to DrasiError at API boundary
//! let error = map_component_error(result, "source", &source_id, "start").unwrap_err();
//! assert!(matches!(error.classification(), DrasiError::OperationFailed { .. }));
//! assert!(error.downcast_ref::<std::io::Error>().is_some());
//! ```

use crate::{
    computation::v1::{ComponentId, ComponentName},
    error::DrasiError,
    lib_core::DrasiLib,
};
use anyhow::Result as AnyhowResult;

impl DrasiLib {
    pub(crate) fn resolve_component_name(
        &self,
        name: &ComponentName,
        component_type: &'static str,
    ) -> crate::error::Result<ComponentId> {
        self.resolve_component_name_inner(name, Some(component_type))
    }

    pub(crate) fn resolve_any_component_name(
        &self,
        name: &ComponentName,
    ) -> crate::error::Result<ComponentId> {
        self.resolve_component_name_inner(name, None)
    }

    fn resolve_component_name_inner(
        &self,
        name: &ComponentName,
        component_type: Option<&'static str>,
    ) -> crate::error::Result<ComponentId> {
        self.state_guard.require_initialized()?;
        self.computation_runtime
            .component_id_by_name(name, component_type)
    }
}

impl crate::computation::runtime::Runtime {
    pub(crate) fn component_id_by_name(
        &self,
        name: &ComponentName,
        component_type: Option<&'static str>,
    ) -> crate::error::Result<ComponentId> {
        let publication = self.control()?.registry_snapshot();
        let node = publication
            .desired
            .component_by_name(name)
            .filter(|node| {
                component_type.is_none_or(|component_type| {
                    crate::computation::runtime::Runtime::public_component_kind(
                        &publication.desired,
                        node,
                    ) == Some(component_type)
                })
            })
            .ok_or_else(|| {
                DrasiError::component_not_found_by_name(component_type.unwrap_or("component"), name)
            })?;
        Ok(node.descriptor.id().clone())
    }
}

// ============================================================================
// Error mapping functions
// ============================================================================

/// Maps `anyhow::Error` from manager operations to `DrasiError`.
///
/// This function converts internal errors to structured `DrasiError` variants
/// at the public API boundary. It retains the cause with an `OperationFailed`
/// classification containing the component and operation.
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
/// ```rust
/// use drasi_lib::component_ops::map_component_error;
/// let source_id = drasi_lib::ComponentId::generate();
/// let result = map_component_error(Ok(42), "source", &source_id, "start");
/// assert_eq!(result.unwrap(), 42);
/// ```
pub fn map_component_error<T>(
    result: AnyhowResult<T>,
    component_type: &str,
    component_id: &ComponentId,
    operation: &str,
) -> crate::error::Result<T> {
    result.map_err(|e| {
        DrasiError::operation_failed(
            component_type,
            crate::error::OperationSubject::Id(component_id.clone()),
            operation,
            e.to_string(),
        )
        .with_cause(e)
    })
}

/// Maps `anyhow::Error` to `DrasiError` for state-related errors.
///
/// This is used for operations where state validation is the primary concern
/// (e.g., checking if dependencies are ready). It retains the cause with an
/// `InvalidState` classification.
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
    _component_id: &ComponentId,
) -> crate::error::Result<T> {
    result.map_err(|e| DrasiError::invalid_state(e.to_string()).with_cause(e))
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
        let mapped = map_component_error(
            result,
            "source",
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001e0")
                .expect("fixture UUID"),
            "start",
        );
        assert!(mapped.is_ok());
        assert_eq!(mapped.unwrap(), 42);
    }

    #[test]
    fn test_map_component_error_failure() {
        let result: AnyhowResult<()> = Err(anyhow!("Connection timeout"));
        let mapped = map_component_error(
            result,
            "query",
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000050")
                .expect("fixture UUID"),
            "stop",
        );
        assert!(mapped.is_err());

        let err = mapped.unwrap_err();
        match err.classification() {
            DrasiError::OperationFailed {
                component_type,
                subject: crate::error::OperationSubject::Id(component_id),
                operation,
                reason,
            } => {
                assert_eq!(component_type, "query");
                assert_eq!(
                    component_id,
                    &crate::ComponentId::try_new("00000000-0000-0000-0000-000000000050").unwrap()
                );
                assert_eq!(operation, "stop");
                assert!(reason.contains("Connection timeout"));
            }
            _ => panic!("Expected OperationFailed variant"),
        }
    }

    #[test]
    fn test_map_state_error_success() {
        let result: AnyhowResult<String> = Ok("success".to_string());
        let mapped = map_state_error(
            result,
            "reaction",
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000021")
                .expect("fixture UUID"),
        );
        assert!(mapped.is_ok());
        assert_eq!(mapped.unwrap(), "success");
    }

    #[test]
    fn test_map_state_error_failure() {
        let result: AnyhowResult<()> = Err(anyhow!("Component is already running"));
        let mapped = map_state_error(
            result,
            "source",
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001e1")
                .expect("fixture UUID"),
        );
        assert!(mapped.is_err());

        let err = mapped.unwrap_err();
        match err.classification() {
            DrasiError::InvalidState { message } => {
                assert!(message.contains("already running"));
            }
            _ => panic!("Expected InvalidState variant"),
        }
    }

    #[test]
    fn component_and_state_mapping_preserve_provider_causes() {
        for state in [false, true] {
            let cause = anyhow::Error::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "provider rejected write",
            ))
            .context("flushing progress");
            let result: AnyhowResult<()> = Err(cause);
            let error = if state {
                map_state_error(
                    result,
                    "source",
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000071")
                        .expect("fixture UUID"),
                )
            } else {
                map_component_error(
                    result,
                    "source",
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000071")
                        .expect("fixture UUID"),
                    "stop",
                )
            }
            .unwrap_err();
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
    }

    #[test]
    fn component_mapping_preserves_graph_and_worker_cleanup_causes() {
        use crate::computation::v1::{ComponentId, GraphError};
        use crate::context::workers::WorkerCleanupError;

        let cause = GraphError::Component {
            component: ComponentId::generate(),
            operation: "stop",
            source: anyhow::Error::new(WorkerCleanupError::TimedOut {
                timeout: std::time::Duration::from_secs(5),
            })
            .context("joining listener"),
        };
        let error = map_component_error::<()>(
            Err(anyhow::Error::new(cause).context("stopping graph")),
            "source",
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000071")
                .expect("fixture UUID"),
            "stop",
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<GraphError>(),
            Some(GraphError::Component {
                operation: "stop",
                ..
            })
        ));
        assert!(matches!(
            error.downcast_ref::<WorkerCleanupError>(),
            Some(WorkerCleanupError::TimedOut { timeout })
                if *timeout == std::time::Duration::from_secs(5)
        ));
    }
}
