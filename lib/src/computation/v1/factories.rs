// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use super::{
    ComponentFactory, ComputationTopologyFactory, ContinuousQueryFactory, FactoryRegistry,
    LegacyReactionFactory, LegacySourceFactory, QueryReplayFactory, QueryResultsOutletFactory,
    ReactionPluginAdapterFactory, SourcePluginAdapterFactory, WalReplaySourceFactory,
};

impl FactoryRegistry {
    /// Built-in native components and legacy plugin adapters.
    pub fn standard() -> Self {
        let factories: Vec<Arc<dyn ComponentFactory>> = vec![
            Arc::new(ContinuousQueryFactory::default()),
            Arc::new(LegacySourceFactory::default()),
            Arc::new(SourcePluginAdapterFactory::default()),
            Arc::new(LegacyReactionFactory::default()),
            Arc::new(ReactionPluginAdapterFactory::default()),
            Arc::new(QueryReplayFactory::default()),
            Arc::new(QueryResultsOutletFactory::default()),
            Arc::new(WalReplaySourceFactory::default()),
            Arc::new(ComputationTopologyFactory::default()),
        ];
        let mut registry = Self::default();
        for factory in factories {
            registry
                .register(factory)
                .expect("standard component implementations are unique");
        }
        registry
    }
}
