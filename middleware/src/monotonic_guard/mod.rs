mod config;
mod factory;

pub use config::MonotonicGuardConfig;
pub use factory::MonotonicGuardFactory;

use async_trait::async_trait;
use drasi_core::{
    interface::{ElementIndex, MiddlewareError, SourceMiddleware},
    models::{Element, ElementValue, SourceChange},
};

/// Enforces "Event-Time-Wins" semantics by dropping stale updates based on timestamp comparison.
pub struct MonotonicGuard {
    timestamp_property: String,
}

impl MonotonicGuard {
    pub fn new(config: MonotonicGuardConfig) -> Self {
        MonotonicGuard {
            timestamp_property: config.timestamp_property,
        }
    }

    fn extract_timestamp(&self, element: &Element) -> i64 {
        if let Some(ElementValue::Integer(ts)) =
            element.get_properties().get(&self.timestamp_property)
        {
            return *ts;
        }
        element.get_effective_from() as i64
    }
}

#[async_trait]
impl SourceMiddleware for MonotonicGuard {
    async fn process(
        &self,
        source_change: SourceChange,
        element_index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        match &source_change {
            SourceChange::Update { element } => {
                let new_timestamp = self.extract_timestamp(element);
                let element_ref = element.get_reference();

                match element_index.get_element(element_ref).await {
                    Ok(Some(existing_element)) => {
                        let old_timestamp = self.extract_timestamp(&existing_element);
                        if new_timestamp > old_timestamp {
                            Ok(vec![source_change])
                        } else {
                            Ok(vec![])
                        }
                    }
                    Ok(None) => Ok(vec![source_change]),
                    Err(e) => Err(MiddlewareError::IndexError(e)),
                }
            }
            SourceChange::Insert { .. }
            | SourceChange::Delete { .. }
            | SourceChange::Future { .. } => Ok(vec![source_change]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::models::{ElementMetadata, ElementPropertyMap, ElementReference};
    use std::sync::Arc;

    fn create_test_element(timestamp: i64) -> Element {
        let mut props = ElementPropertyMap::default();
        props.insert("last_modified_at", ElementValue::Integer(timestamp));

        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", "test_id"),
                labels: vec![Arc::from("TestNode")].into(),
                effective_from: 0,
            },
            properties: props,
        }
    }

    #[test]
    fn test_extract_timestamp_from_property() {
        let config = MonotonicGuardConfig {
            timestamp_property: "last_modified_at".to_string(),
        };
        let guard = MonotonicGuard::new(config);
        let element = create_test_element(12345);

        assert_eq!(guard.extract_timestamp(&element), 12345);
    }

    #[test]
    fn test_extract_timestamp_fallback_to_effective_from() {
        let config = MonotonicGuardConfig {
            timestamp_property: "missing_property".to_string(),
        };
        let guard = MonotonicGuard::new(config);

        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", "test_id"),
                labels: vec![Arc::from("TestNode")].into(),
                effective_from: 99999,
            },
            properties: ElementPropertyMap::default(),
        };

        assert_eq!(guard.extract_timestamp(&element), 99999);
    }
}
