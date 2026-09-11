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

use crate::{
    config::{DrasiLibConfig, RuntimeConfig},
    DrasiLib, Source,
};
use std::{collections::HashMap, ops::Deref, sync::Arc};

/// The same bare-library fixture used by manager scenarios, without the
/// application builder's built-in topology source. Only construction varies.
pub(crate) async fn core(
    indexes: Arc<crate::indexes::IndexFactory>,
    middleware: Arc<drasi_core::middleware::MiddlewareTypeRegistry>,
    recovery: Option<crate::RecoveryPolicy>,
) -> Arc<DrasiLib> {
    let mut config = RuntimeConfig::new(
        DrasiLibConfig {
            id: "test-instance".into(),
            ..Default::default()
        },
        HashMap::new(),
        None,
        None,
        None,
        recovery,
        None,
    );
    config.index_factory = indexes;
    let mut core = DrasiLib::new(Arc::new(config));
    core.middleware_registry = middleware.clone();
    core.query_manager = Arc::new(crate::queries::QueryManager::new(
        "test-instance",
        core.source_manager.clone(),
        core.config.index_factory.clone(),
        middleware,
        core.log_registry.clone(),
        core.component_graph.clone(),
        core.component_graph.read().await.update_sender(),
        recovery,
    ));
    core.inspection = crate::inspection::InspectionAPI::new(
        core.source_manager.clone(),
        core.query_manager.clone(),
        core.reaction_manager.clone(),
        core.state_guard.clone(),
        core.config.clone(),
    );
    core.lifecycle = Arc::new(crate::lifecycle::LifecycleManager::new(
        core.config.clone(),
        core.source_manager.clone(),
        core.query_manager.clone(),
        core.reaction_manager.clone(),
        core.component_graph.clone(),
    ));
    #[cfg(feature = "computation")]
    if super::execution_mode() == crate::ExecutionMode::ComputationGraph {
        let runtime = crate::computation::compatibility::Runtime::new(&core, None)
            .await
            .expect("native fixture");
        core.inspection.set_computation(runtime.clone());
        core.computation_runtime = Some(runtime);
        core.state_guard.mark_initialized();
        return Arc::new(core);
    }
    core.initialize().await.expect("legacy fixture");
    Arc::new(core)
}

pub(crate) async fn empty_core() -> Arc<DrasiLib> {
    core(
        Arc::new(crate::indexes::IndexFactory::new(vec![], HashMap::new())),
        Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
        None,
    )
    .await
}

pub(crate) struct SourceManager(pub Arc<DrasiLib>);
impl Deref for SourceManager {
    type Target = crate::sources::SourceManager;
    fn deref(&self) -> &Self::Target {
        &self.0.source_manager
    }
}
impl SourceManager {
    pub(crate) async fn add(&self, source: impl Source + 'static) -> anyhow::Result<()> {
        Ok(self.0.add_source(source).await?)
    }
    pub(crate) async fn delete(&self, id: &str, cleanup: bool) -> anyhow::Result<()> {
        Ok(self.0.remove_source(id, cleanup).await?)
    }
    pub(crate) async fn start_source(&self, id: String) -> anyhow::Result<()> {
        Ok(self.0.start_source(&id).await?)
    }
    pub(crate) async fn stop_source(&self, id: String) -> anyhow::Result<()> {
        Ok(self.0.stop_source(&id).await?)
    }
    pub(crate) async fn update_source(
        &self,
        id: String,
        source: impl Source + 'static,
    ) -> anyhow::Result<()> {
        Ok(self.0.update_source(&id, source).await?)
    }
    pub(crate) async fn start_all(&self) -> anyhow::Result<()> {
        #[cfg(feature = "computation")]
        if let Some(runtime) = &self.0.computation_runtime {
            return runtime.start_kind("source").await;
        }
        self.0.source_manager.start_all().await
    }
}

pub(crate) struct QueryManager(pub Arc<DrasiLib>);
impl Deref for QueryManager {
    type Target = crate::queries::QueryManager;
    fn deref(&self) -> &Self::Target {
        &self.0.query_manager
    }
}

pub(crate) struct ReactionManager(pub Arc<DrasiLib>);
impl Deref for ReactionManager {
    type Target = crate::reactions::ReactionManager;
    fn deref(&self) -> &Self::Target {
        &self.0.reaction_manager
    }
}
impl ReactionManager {
    pub(crate) async fn add(&self, reaction: impl crate::Reaction + 'static) -> anyhow::Result<()> {
        #[cfg(feature = "computation")]
        if let Some(runtime) = &self.0.computation_runtime {
            return runtime.declare_reaction(Box::new(reaction)).await;
        }
        {
            let mut graph = self.0.component_graph.write().await;
            let queries = reaction.query_ids();
            for query in &queries {
                if !graph.contains(query) {
                    graph.register_query(query, HashMap::new(), &[])?;
                }
            }
            graph.register_reaction(
                reaction.id(),
                HashMap::from([("kind".into(), reaction.type_name().into())]),
                &queries,
            )?;
        }
        self.0.reaction_manager.provision_reaction(reaction).await
    }
    pub(crate) async fn delete(&self, id: &str, cleanup: bool) -> anyhow::Result<()> {
        Ok(self.0.remove_reaction(id, cleanup).await?)
    }
    pub(crate) async fn start_reaction(&self, id: String) -> anyhow::Result<()> {
        Ok(self.0.start_reaction(&id).await?)
    }
    pub(crate) async fn stop_reaction(&self, id: String) -> anyhow::Result<()> {
        Ok(self.0.stop_reaction(&id).await?)
    }
    pub(crate) async fn update_reaction(
        &self,
        id: String,
        reaction: impl crate::Reaction + 'static,
    ) -> anyhow::Result<()> {
        Ok(self.0.update_reaction(&id, reaction).await?)
    }
    pub(crate) async fn start_all(&self) -> anyhow::Result<()> {
        #[cfg(feature = "computation")]
        if let Some(runtime) = &self.0.computation_runtime {
            return runtime.start_kind("reaction").await;
        }
        self.0.reaction_manager.start_all().await
    }
}
impl QueryManager {
    pub(crate) async fn get_query_instance(
        &self,
        id: &str,
    ) -> anyhow::Result<Arc<dyn crate::queries::Query>> {
        let query = self
            .0
            .query_manager
            .get_query_instance(id)
            .await
            .map_err(anyhow::Error::msg)?;
        assert_query_implementation(&self.0, query.as_ref());
        Ok(query)
    }
    pub(crate) async fn provision_query(
        &self,
        config: crate::config::QueryConfig,
    ) -> anyhow::Result<()> {
        #[cfg(feature = "computation")]
        if let Some(runtime) = &self.0.computation_runtime {
            return runtime.declare_query(config).await;
        }

        self.0.query_manager.provision_query(config).await
    }
    pub(crate) async fn teardown_query(&self, id: String) -> anyhow::Result<()> {
        #[cfg(feature = "computation")]
        if self.0.computation_runtime.is_some() {
            return Ok(self.0.remove_query(&id).await?);
        }
        self.0.query_manager.teardown_query(id).await
    }
    pub(crate) async fn declare(&self, config: crate::config::QueryConfig) -> anyhow::Result<()> {
        #[cfg(feature = "computation")]
        if let Some(runtime) = &self.0.computation_runtime {
            let result = runtime.declare_query(config).await;
            if let Err(error) = &result {
                log::error!("Query fixture declaration failed: {error:#}");
            }
            return result;
        }
        {
            let mut graph = self.0.component_graph.write().await;
            let sources: Vec<_> = config
                .sources
                .iter()
                .map(|source| source.source_id.clone())
                .collect();
            for source in &sources {
                if !graph.contains(source) {
                    graph.register_source(source, HashMap::new())?;
                }
            }
            graph.register_query(
                &config.id,
                HashMap::from([("query".into(), config.query.clone())]),
                &sources,
            )?;
        }
        self.0.query_manager.provision_query(config).await
    }
    pub(crate) async fn add(&self, config: crate::config::QueryConfig) -> anyhow::Result<()> {
        Ok(self.0.add_query(config).await?)
    }
    pub(crate) async fn delete(&self, id: &str) -> anyhow::Result<()> {
        Ok(self.0.remove_query(id).await?)
    }
    pub(crate) async fn start_query(&self, id: String) -> anyhow::Result<()> {
        Ok(self.0.start_query(&id).await?)
    }
    pub(crate) async fn stop_query(&self, id: String) -> anyhow::Result<()> {
        Ok(self.0.stop_query(&id).await?)
    }
    pub(crate) async fn update_query(
        &self,
        id: String,
        config: crate::config::QueryConfig,
    ) -> anyhow::Result<()> {
        Ok(self.0.update_query(&id, config).await?)
    }
}

pub(crate) fn assert_query_implementation(core: &DrasiLib, query: &dyn crate::queries::Query) {
    match core.execution_mode() {
        crate::ExecutionMode::ComponentGraph => assert!(
            query.as_any().is::<crate::queries::DrasiQuery>(),
            "component fixture must use the original query implementation"
        ),
        #[cfg(feature = "computation")]
        crate::ExecutionMode::ComputationGraph => assert!(
            query
                .as_any()
                .is::<crate::computation::compatibility::QueryInstance>(),
            "native fixture must execute a native query, not delegate to DrasiQuery"
        ),
    }
}
