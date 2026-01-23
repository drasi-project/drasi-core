use std::sync::Arc;

use drasi_core::{
    interface::{MiddlewareSetupError, SourceMiddleware, SourceMiddlewareFactory},
    models::SourceMiddlewareConfig,
};

use super::{MonotonicGuard, MonotonicGuardConfig};

pub struct MonotonicGuardFactory;

impl MonotonicGuardFactory {
    pub fn new() -> Self {
        MonotonicGuardFactory
    }
}

impl Default for MonotonicGuardFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceMiddlewareFactory for MonotonicGuardFactory {
    fn name(&self) -> String {
        "monotonic-guard".to_string()
    }

    fn create(
        &self,
        config: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        let guard_config: MonotonicGuardConfig = serde_json::from_value(serde_json::Value::Object(
            config.config.clone(),
        ))
        .map_err(|e| {
            MiddlewareSetupError::InvalidConfiguration(format!(
                "Failed to parse MonotonicGuard configuration: {}",
                e
            ))
        })?;

        if guard_config.timestamp_property.is_empty() {
            return Err(MiddlewareSetupError::InvalidConfiguration(
                "timestamp_property cannot be empty".to_string(),
            ));
        }

        Ok(Arc::new(MonotonicGuard::new(guard_config)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_factory_create_valid_config() {
        let factory = MonotonicGuardFactory::new();
        assert_eq!(factory.name(), "monotonic-guard");

        let config = SourceMiddlewareConfig {
            kind: Arc::from("monotonic-guard"),
            name: Arc::from("test_guard"),
            config: json!({
                "timestamp_property": "last_modified_at"
            })
            .as_object()
            .unwrap()
            .clone(),
        };

        let result = factory.create(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_factory_create_empty_timestamp_property() {
        let factory = MonotonicGuardFactory::new();

        let config = SourceMiddlewareConfig {
            kind: Arc::from("monotonic-guard"),
            name: Arc::from("test_guard"),
            config: json!({
                "timestamp_property": ""
            })
            .as_object()
            .unwrap()
            .clone(),
        };

        let result = factory.create(&config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("cannot be empty"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn test_factory_create_invalid_config() {
        let factory = MonotonicGuardFactory::new();

        let config = SourceMiddlewareConfig {
            kind: Arc::from("monotonic-guard"),
            name: Arc::from("test_guard"),
            config: json!({
                "invalid_field": "value"
            })
            .as_object()
            .unwrap()
            .clone(),
        };

        let result = factory.create(&config);
        assert!(result.is_err());
    }
}
