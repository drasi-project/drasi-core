use std::{collections::HashMap, sync::Arc};

use crate::{
    interface::{MiddlewareError, MiddlewareSetupError, SourceMiddleware, SourceMiddlewareFactory},
    models::{SourceChange, SourceMiddlewareConfig},
};

pub struct MiddlewareTypeRegistry {
    source_middleware_types: HashMap<String, Arc<dyn SourceMiddlewareFactory>>,
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
        self.source_middleware_types.get(name).map(|f| f.clone())
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
            source_instances.insert(Arc::from(config.name.clone()), instance);
        }

        Ok(MiddlewareContainer { source_instances })
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn SourceMiddleware>> {
        self.source_instances.get(name).map(|f| f.clone())
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
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        let mut source_changes = vec![source_change];

        for middleware in &self.pipeline {
            let mut new_source_changes = Vec::new();
            for source_change in source_changes {
                new_source_changes.append(&mut middleware.process(source_change).await?);
            }

            source_changes = new_source_changes;
        }

        Ok(source_changes)
    }
}

pub struct SourceMiddlewarePipelineCollection {
    items: HashMap<Arc<str>, Arc<SourceMiddlewarePipeline>>,
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
        self.items.get(&source_id).map(|p| p.clone())
    }
}
