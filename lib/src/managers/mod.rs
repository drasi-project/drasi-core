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

//! Shared utilities for component managers (SourceManager, QueryManager, ReactionManager)

mod component_log;
mod event_history;
pub(crate) mod lifecycle_helpers;
mod logging;
mod tracing_layer;

pub use component_log::*;
pub use event_history::*;
pub use logging::*;
pub use tracing_layer::*;

/// Typed "not found" error for component lookups.
///
/// Used inside managers (Layer 2) as an `anyhow` inner error so the public API
/// boundary can `downcast_ref` instead of parsing error message strings.
#[derive(Debug, thiserror::Error)]
#[error("{component_type} not found: {component_id}")]
pub struct ComponentNotFoundError {
    pub component_type: &'static str,
    pub component_id: String,
}

impl ComponentNotFoundError {
    pub fn new(component_type: &'static str, component_id: impl Into<String>) -> Self {
        Self {
            component_type,
            component_id: component_id.into(),
        }
    }
}
