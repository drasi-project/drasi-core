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

pub mod fs_plugin_loader;
pub mod plugin_loader;

use std::{collections::HashMap, sync::Arc};

use crate::{
    interface::{
        ElementIndex, MiddlewareError, MiddlewareSetupError, SourceMiddleware,
        SourceMiddlewareFactory,
    },
    models::{SourceChange, SourceMiddlewareConfig},
};

use self::plugin_loader::{PluginLoadError, PluginLoader};

pub struct MiddlewareTypeRegistry {
    source_middleware_types: HashMap<String, Arc<dyn SourceMiddlewareFactory>>,
    plugin_loaders: Vec<Box<dyn PluginLoader>>,
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
            plugin_loaders: Vec::new(),
        }
    }

    /// Registers a middleware factory
    pub fn register(&mut self, factory: Arc<dyn SourceMiddlewareFactory>) {
        self.source_middleware_types.insert(factory.name(), factory);
    }

    /// Registers a plugin loader that can discover and load external middleware plugins
    pub fn register_plugin_loader(&mut self, loader: Box<dyn PluginLoader>) {
        self.plugin_loaders.push(loader);
    }

    /// Discovers and loads all plugins from registered plugin loaders
    pub fn discover_and_load_plugins(&mut self) -> Result<usize, PluginLoadError> {
        let mut loaded_count = 0;
        let mut factories_to_register = Vec::new();

        // Process each loader
        for loader in &mut self.plugin_loaders {
            // Discover plugins from this loader
            let descriptors = loader.discover_plugins()?;
            
            // Load each discovered plugin
            for descriptor in descriptors {
                match loader.load_plugin(&descriptor) {
                    Ok(factory) => {
                        log::info!(
                            "Loaded plugin '{}' version {} from {}",
                            descriptor.name,
                            descriptor.version,
                            descriptor.path.display()
                        );
                        factories_to_register.push(factory);
                        loaded_count += 1;
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to load plugin '{}' from {}: {}",
                            descriptor.name,
                            descriptor.path.display(),
                            e
                        );
                    }
                }
            }
        }

        // Register all loaded factories
        for factory in factories_to_register {
            self.register(factory);
        }

        Ok(loaded_count)
    }

    /// Gets a middleware factory by name
    pub fn get(&self, name: &str) -> Option<Arc<dyn SourceMiddlewareFactory>> {
        self.source_middleware_types.get(name).cloned()
    }

    /// Returns the number of registered middleware factories
    pub fn count(&self) -> usize {
        self.source_middleware_types.len()
    }

    /// Lists all registered middleware names
    pub fn list_middleware_names(&self) -> Vec<String> {
        self.source_middleware_types.keys().cloned().collect()
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
                        "Unknown middleware: {name}"
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
                new_source_changes.append(
                    &mut middleware
                        .process(source_change, element_index.as_ref())
                        .await?,
                );
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
