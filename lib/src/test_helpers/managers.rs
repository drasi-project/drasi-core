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
/// application builder's built-in topology source.
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
    let mut core = DrasiLib::new_with_middleware(Arc::new(config), middleware);
    core.initialize().await.expect("computation fixture");
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
        self.0.source_manager()
    }
}
impl SourceManager {
    pub(crate) async fn get_source_instance(&self, id: &str) -> Option<Arc<dyn Source>> {
        match self.0.source_instance(id).await {
            Ok(source) => Some(source),
            Err(error)
                if error
                    .downcast_ref::<crate::managers::ComponentNotFoundError>()
                    .is_some() =>
            {
                None
            }
            Err(error) => panic!("source fixture lookup failed: {error:#}"),
        }
    }

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
        self.0.computation_runtime.start_kind("source").await
    }
}

pub(crate) struct QueryManager(pub Arc<DrasiLib>);
impl Deref for QueryManager {
    type Target = crate::queries::QueryManager;
    fn deref(&self) -> &Self::Target {
        self.0.query_manager()
    }
}

pub(crate) struct ReactionManager(pub Arc<DrasiLib>);
impl Deref for ReactionManager {
    type Target = crate::reactions::ReactionManager;
    fn deref(&self) -> &Self::Target {
        self.0.reaction_manager()
    }
}
impl ReactionManager {
    pub(crate) async fn add(&self, reaction: impl crate::Reaction + 'static) -> anyhow::Result<()> {
        self.0
            .computation_runtime
            .declare_reaction(Box::new(reaction))
            .await
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
        self.0.computation_runtime.start_kind("reaction").await
    }
}
impl QueryManager {
    pub(crate) async fn get_query_instance(
        &self,
        id: &str,
    ) -> anyhow::Result<Arc<dyn crate::queries::Query>> {
        let query = self.0.computation_runtime.query(id).await?;
        assert_query_implementation(&self.0, query.as_ref());
        Ok(query)
    }
    pub(crate) async fn provision_query(
        &self,
        config: crate::config::QueryConfig,
    ) -> anyhow::Result<()> {
        self.0.computation_runtime.declare_query(config).await
    }
    pub(crate) async fn teardown_query(&self, id: String) -> anyhow::Result<()> {
        Ok(self.0.remove_query(&id).await?)
    }
    pub(crate) async fn declare(&self, config: crate::config::QueryConfig) -> anyhow::Result<()> {
        self.0.computation_runtime.declare_query(config).await
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
    pub(crate) async fn start_query_and_wait(&self, id: String) -> anyhow::Result<()> {
        self.0.start_query(&id).await?;
        let handle = self.0.computation_component(&id)?;
        tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_started()).await??;
        Ok(())
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

pub(crate) fn assert_query_implementation(_core: &DrasiLib, query: &dyn crate::queries::Query) {
    assert!(
        query
            .as_any()
            .is::<crate::computation::runtime::QueryInstance>(),
        "queries must execute through the computation runtime"
    );
}
