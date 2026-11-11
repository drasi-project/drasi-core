// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use async_trait::async_trait;
use drasi_core::{
    interface::{
        ElementIndex, MiddlewareError, MiddlewareSetupError, SourceMiddleware,
        SourceMiddlewareFactory,
    },
    middleware::MiddlewareTypeRegistry,
    models::{Element, ElementValue, SourceChange, SourceMiddlewareConfig},
};

struct MarkerFactory(AtomicU64);
struct Marker(u64);

impl SourceMiddlewareFactory for MarkerFactory {
    fn name(&self) -> String {
        "query-custom".into()
    }

    fn create(
        &self,
        _: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        Ok(Arc::new(Marker(self.0.fetch_add(1, Ordering::Relaxed) + 1)))
    }
}

impl Marker {
    async fn mark(
        &self,
        element: &mut Element,
        index: &dyn ElementIndex,
    ) -> Result<(), MiddlewareError> {
        let properties = match &*element {
            Element::Node { properties, .. } | Element::Relation { properties, .. } => properties,
        };
        if properties.get("fail") == Some(&ElementValue::Bool(true)) {
            return Err(MiddlewareError::SourceChangeError(
                "injected middleware failure".into(),
            ));
        }
        let previous = index.get_element(&element.get_metadata().reference).await?;
        let count = match previous.as_deref() {
            Some(Element::Node { properties, .. } | Element::Relation { properties, .. }) => {
                match properties.get("visits") {
                    Some(ElementValue::Integer(value)) => *value,
                    _ => 0,
                }
            }
            None => 0,
        };
        let properties = match element {
            Element::Node { properties, .. } | Element::Relation { properties, .. } => properties,
        };
        properties.insert("visits", ElementValue::Integer(count + 1));
        properties.insert("factory", ElementValue::Integer(self.0 as i64));
        Ok(())
    }
}

#[async_trait]
impl SourceMiddleware for Marker {
    async fn process(
        &self,
        mut change: SourceChange,
        index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        match &mut change {
            SourceChange::Insert { element } | SourceChange::Replace { element } => {
                self.mark(element, index).await?
            }
            SourceChange::Update {
                element,
                removed_properties,
            } => {
                self.mark(element, index).await?;
                removed_properties.remove("visits");
                removed_properties.remove("factory");
            }
            SourceChange::Delete { .. } | SourceChange::Future { .. } => {}
        }
        Ok(vec![change])
    }
}

fn plugin() -> anyhow::Result<drasi_computation_plugin_sdk::PluginDefinition> {
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(MarkerFactory(AtomicU64::new(0))));
    drasi_computation_transformer_query::plugin_with_middleware_registry(Arc::new(registry))
}

drasi_computation_plugin_sdk::export_computation_plugin!(plugin());
