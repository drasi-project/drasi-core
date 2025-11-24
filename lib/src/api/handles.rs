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

//! Handle registry for managing application source and reaction handles

use crate::api::{DrasiError, Result};
use crate::reactions::ApplicationReactionHandle;
use crate::sources::ApplicationSourceHandle;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Registry for managing application handles
#[derive(Clone)]
pub struct HandleRegistry {
    source_handles: Arc<RwLock<HashMap<String, ApplicationSourceHandle>>>,
    reaction_handles: Arc<RwLock<HashMap<String, ApplicationReactionHandle>>>,
}

impl HandleRegistry {
    /// Create a new handle registry
    pub fn new() -> Self {
        Self {
            source_handles: Arc::new(RwLock::new(HashMap::new())),
            reaction_handles: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a source handle
    pub async fn register_source_handle(&self, id: String, handle: ApplicationSourceHandle) {
        self.source_handles.write().await.insert(id, handle);
    }

    /// Register a reaction handle
    pub async fn register_reaction_handle(&self, id: String, handle: ApplicationReactionHandle) {
        self.reaction_handles.write().await.insert(id, handle);
    }

    /// Get a source handle by ID
    pub async fn get_source_handle(&self, id: &str) -> Result<ApplicationSourceHandle> {
        self.source_handles
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| DrasiError::component_not_found("source", id))
    }

    /// Get a reaction handle by ID
    pub async fn get_reaction_handle(&self, id: &str) -> Result<ApplicationReactionHandle> {
        self.reaction_handles
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| DrasiError::component_not_found("reaction", id))
    }

    /// Check if a source handle exists
    pub async fn has_source_handle(&self, id: &str) -> bool {
        self.source_handles.read().await.contains_key(id)
    }

    /// Check if a reaction handle exists
    pub async fn has_reaction_handle(&self, id: &str) -> bool {
        self.reaction_handles.read().await.contains_key(id)
    }

    /// Remove a source handle
    pub async fn remove_source_handle(&self, id: &str) -> Option<ApplicationSourceHandle> {
        self.source_handles.write().await.remove(id)
    }

    /// Remove a reaction handle
    pub async fn remove_reaction_handle(&self, id: &str) -> Option<ApplicationReactionHandle> {
        self.reaction_handles.write().await.remove(id)
    }
}

impl Default for HandleRegistry {
    fn default() -> Self {
        Self::new()
    }
}
