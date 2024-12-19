// Copyright 2024 The Drasi Authors.
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

use std::{collections::HashMap, sync::Arc};

use crate::{
    interface::{ElementIndex, MiddlewareError, MiddlewareSetupError, SourceMiddleware, SourceMiddlewareFactory},
    models::{SourceChange, SourceMiddlewareConfig},
};

pub struct MiddlewareTypeRegistry {
    source_middleware_types: HashMap<String, Arc<dyn SourceMiddlewareFactory>>,
}

impl Default for MiddlewareTypeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MiddlewareTypeRegistry {
    pub fn new() -> Self {
        MiddlewareTypeRegistry {
            source_middleware_types: HashMap::new(),
        }
    }

    pub fn register(&mut self, factory: Arc<dyn SourceMiddlewareFactory>) {
        self.source_middleware_types.insert(factory.name(), factory);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn SourceMiddlewareFactory>> {
        self.source_middleware_types.get(name).cloned()
    }
}

pub struct MiddlewareContainer {
    source_instances: HashMap<Arc<str>, Arc<dyn SourceMiddleware>>,
}

impl MiddlewareContainer {
    pub fn new(
        registry: &MiddlewareTypeRegistry,
        source_configs: Vec<Arc<SourceMiddlewareConfig>>,
    ) -> Result<Self, MiddlewareSetupError> {
        let mut source_instances = HashMap::new();
        for config in source_configs {
            let factory =
                registry
                    .get(&config.kind)
                    .ok_or(MiddlewareSetupError::InvalidConfiguration(format!(
                        "Unknown middleware kind: {}",
                        config.kind
                    )))?;
            let instance = factory.create(&config)?;
            source_instances.insert(config.name.clone(), instance);
        }

        Ok(MiddlewareContainer { source_instances })
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn SourceMiddleware>> {
        self.source_instances.get(name).cloned()
    }
}

pub struct SourceMiddlewarePipeline {
    pipeline: Vec<Arc<dyn SourceMiddleware>>,
}

impl SourceMiddlewarePipeline {
    pub fn new(
        container: &MiddlewareContainer,
        pipeline_keys: Vec<Arc<str>>,
    ) -> Result<Self, MiddlewareSetupError> {
        let pipeline = pipeline_keys
            .iter()
            .map(|name| {
                container
                    .get(name)
                    .ok_or(MiddlewareSetupError::InvalidConfiguration(format!(
                        "Unknown middleware: {}",
                        name
                    )))
            })
            .collect::<Result<Vec<Arc<dyn SourceMiddleware>>, MiddlewareSetupError>>()?;

        Ok(SourceMiddlewarePipeline { pipeline })
    }

    pub async fn process(
        &self,
        source_change: SourceChange,
        element_index: Arc<dyn ElementIndex>,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        let mut source_changes = vec![source_change];

        for middleware in &self.pipeline {
            let mut new_source_changes = Vec::new();
            for source_change in source_changes {
                new_source_changes.append(&mut middleware.process(source_change, element_index.as_ref()).await?);
            }

            source_changes = new_source_changes;
        }

        Ok(source_changes)
    }
}

pub struct SourceMiddlewarePipelineCollection {
    items: HashMap<Arc<str>, Arc<SourceMiddlewarePipeline>>,
}

impl Default for SourceMiddlewarePipelineCollection {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceMiddlewarePipelineCollection {
    pub fn new() -> Self {
        SourceMiddlewarePipelineCollection {
            items: HashMap::new(),
        }
    }

    pub fn insert(&mut self, source_id: Arc<str>, pipeline: SourceMiddlewarePipeline) {
        self.items.insert(source_id, Arc::new(pipeline));
    }

    pub fn get(&self, source_id: Arc<str>) -> Option<Arc<SourceMiddlewarePipeline>> {
        self.items.get(&source_id).cloned()
    }
}
