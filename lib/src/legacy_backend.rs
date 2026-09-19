// Copyright 2026 The Drasi Authors.
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

use std::sync::{Arc, OnceLock};
use tokio::sync::RwLock;

use crate::{
    component_graph::{ComponentGraph, ComponentUpdateSender},
    config::RuntimeConfig,
    lifecycle::LifecycleManager,
    managers::ComponentLogRegistry,
    queries::QueryManager,
    reactions::ReactionManager,
    sources::SourceManager,
};
use drasi_core::middleware::MiddlewareTypeRegistry;

pub(crate) struct LegacyBackend {
    pub source_manager: Arc<SourceManager>,
    pub query_manager: Arc<QueryManager>,
    pub reaction_manager: Arc<ReactionManager>,
    pub lifecycle: LifecycleManager,
}

/// Native ordinary APIs never initialize this backend. The legacy execution
/// path and explicit legacy-manager access share its single lazy instance.
pub(crate) struct LegacyBackendHost {
    backend: OnceLock<LegacyBackend>,
    config: Arc<RuntimeConfig>,
    middleware: Arc<MiddlewareTypeRegistry>,
    logs: Arc<ComponentLogRegistry>,
    graph: Arc<RwLock<ComponentGraph>>,
    updates: ComponentUpdateSender,
}

impl LegacyBackendHost {
    pub(crate) fn new(
        config: Arc<RuntimeConfig>,
        middleware: Arc<MiddlewareTypeRegistry>,
        logs: Arc<ComponentLogRegistry>,
        graph: Arc<RwLock<ComponentGraph>>,
        updates: ComponentUpdateSender,
    ) -> Self {
        Self {
            backend: OnceLock::new(),
            config,
            middleware,
            logs,
            graph,
            updates,
        }
    }

    pub(crate) fn initialized(&self) -> Option<&LegacyBackend> {
        self.backend.get()
    }

    pub(crate) fn get(&self) -> &LegacyBackend {
        self.backend.get_or_init(|| {
            let source_manager = Arc::new(SourceManager::new(
                &self.config.id,
                self.logs.clone(),
                self.graph.clone(),
                self.updates.clone(),
            ));
            let query_manager = Arc::new(QueryManager::new(
                &self.config.id,
                source_manager.clone(),
                self.config.index_factory.clone(),
                self.middleware.clone(),
                self.logs.clone(),
                self.graph.clone(),
                self.updates.clone(),
                self.config.default_recovery_policy,
            ));
            let reaction_manager = Arc::new(ReactionManager::new(
                &self.config.id,
                self.logs.clone(),
                self.graph.clone(),
                self.updates.clone(),
            ));
            let lifecycle = LifecycleManager::new(
                self.config.clone(),
                source_manager.clone(),
                query_manager.clone(),
                reaction_manager.clone(),
                self.graph.clone(),
            );
            LegacyBackend {
                source_manager,
                query_manager,
                reaction_manager,
                lifecycle,
            }
        })
    }
}
