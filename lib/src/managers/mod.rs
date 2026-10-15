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

//! Compatibility imports for the former manager utilities.
//!
//! New code should use [`crate::observability`] for logs and event history, and
//! [`crate::error::ComponentNotFoundError`] for typed lookup failures.

pub use crate::error::ComponentNotFoundError;
pub use crate::observability::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::TypeId;

    #[test]
    fn compatibility_imports_share_observability_and_error_types() {
        assert_eq!(
            TypeId::of::<ComponentLogRegistry>(),
            TypeId::of::<crate::observability::ComponentLogRegistry>()
        );
        assert_eq!(
            TypeId::of::<ComponentLogKey>(),
            TypeId::of::<crate::observability::ComponentLogKey>()
        );
        assert_eq!(
            TypeId::of::<LogMessage>(),
            TypeId::of::<crate::observability::LogMessage>()
        );
        assert_eq!(
            TypeId::of::<ComponentEventHistory>(),
            TypeId::of::<crate::observability::ComponentEventHistory>()
        );
        let error = anyhow::Error::new(ComponentNotFoundError::new("source", "missing"));
        let retained = error
            .downcast_ref::<crate::error::ComponentNotFoundError>()
            .unwrap();
        assert_eq!(retained.component_id, "missing");
        assert_eq!(error.to_string(), "source not found: missing");
    }
}
