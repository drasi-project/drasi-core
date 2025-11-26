#[cfg(test)]
mod tests {
    use super::super::schema::*;
    use crate::channels::DispatchMode;
    use serde_json::json;
    use std::collections::HashMap;

    /// Helper to convert a serde_json::Value object to HashMap<String, serde_json::Value>
    fn to_hashmap(value: serde_json::Value) -> HashMap<String, serde_json::Value> {
        match value {
            serde_json::Value::Object(map) => map.into_iter().collect(),
            _ => HashMap::new(),
        }
    }

    #[test]
    fn test_source_config_with_dispatch_mode() {
        let config = SourceConfig {
            id: "test_source".to_string(),
            auto_start: true,
            config: crate::config::SourceSpecificConfig::Mock(to_hashmap(json!({
                "data_type": "counter",
                "interval_ms": 1000
            }))),
            bootstrap_provider: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: Some(DispatchMode::Channel),
        };

        assert_eq!(config.id, "test_source");
        assert_eq!(config.dispatch_mode, Some(DispatchMode::Channel));
    }

    #[test]
    fn test_source_config_without_dispatch_mode() {
        let config = SourceConfig {
            id: "test_source".to_string(),
            auto_start: true,
            config: crate::config::SourceSpecificConfig::Mock(to_hashmap(json!({
                "data_type": "counter",
                "interval_ms": 1000
            }))),
            bootstrap_provider: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
        };

        assert_eq!(config.id, "test_source");
        assert_eq!(config.dispatch_mode, None);
    }

    #[test]
    fn test_query_config_with_dispatch_mode() {
        let yaml = r#"
            id: test_query
            query: "RETURN 1"
            source_subscriptions:
              - source_id: source1
                pipeline: []
            dispatch_mode: broadcast
        "#;

        let config: QueryConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.id, "test_query");
        assert_eq!(config.dispatch_mode, Some(DispatchMode::Broadcast));
    }

    #[test]
    fn test_query_config_without_dispatch_mode() {
        let yaml = r#"
            id: test_query
            query: "RETURN 1"
            source_subscriptions:
              - source_id: source1
                pipeline: []
        "#;

        let config: QueryConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.id, "test_query");
        assert_eq!(config.dispatch_mode, None);
    }

    #[test]
    fn test_full_config_with_mixed_dispatch_modes() {
        let mut config = DrasiLibConfig::default();

        // Add sources with different dispatch modes
        config.sources.push(SourceConfig {
            id: "broadcast_source".to_string(),
            auto_start: true,
            config: crate::config::SourceSpecificConfig::Mock(to_hashmap(json!({
                "data_type": "counter",
                "interval_ms": 1000
            }))),
            bootstrap_provider: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: Some(DispatchMode::Broadcast),
        });

        config.sources.push(SourceConfig {
            id: "channel_source".to_string(),
            auto_start: true,
            config: crate::config::SourceSpecificConfig::Mock(to_hashmap(json!({
                "data_type": "sensor",
                "interval_ms": 1000
            }))),
            bootstrap_provider: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: Some(DispatchMode::Channel),
        });

        config.sources.push(SourceConfig {
            id: "default_source".to_string(),
            auto_start: true,
            config: crate::config::SourceSpecificConfig::Mock(to_hashmap(json!({
                "data_type": "counter",
                "interval_ms": 1000
            }))),
            bootstrap_provider: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
        });

        // Add queries with different dispatch modes
        config.queries.push(QueryConfig {
            id: "query1".to_string(),
            query: "RETURN 1".to_string(),
            query_language: crate::config::QueryLanguage::Cypher,
            middleware: vec![],
            source_subscriptions: vec![crate::config::SourceSubscriptionConfig {
                source_id: "broadcast_source".to_string(),
                pipeline: vec![],
            }],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: Some(DispatchMode::Channel),
            storage_backend: None,
        });

        config.queries.push(QueryConfig {
            id: "query2".to_string(),
            query: "RETURN 2".to_string(),
            query_language: crate::config::QueryLanguage::Cypher,
            middleware: vec![],
            source_subscriptions: vec![crate::config::SourceSubscriptionConfig {
                source_id: "channel_source".to_string(),
                pipeline: vec![],
            }],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
        });

        // Add reaction
        config.reactions.push(ReactionConfig {
            id: "reaction1".to_string(),
            queries: vec!["query1".to_string()],
            auto_start: true,
            config: crate::config::ReactionSpecificConfig::Log(to_hashmap(json!({
                "log_level": "info"
            }))),
            priority_queue_capacity: None,
        });

        assert_eq!(config.sources.len(), 3);
        assert_eq!(
            config.sources[0].dispatch_mode,
            Some(DispatchMode::Broadcast)
        );
        assert_eq!(config.sources[1].dispatch_mode, Some(DispatchMode::Channel));
        assert_eq!(config.sources[2].dispatch_mode, None);

        assert_eq!(config.queries.len(), 2);
        assert_eq!(config.queries[0].dispatch_mode, Some(DispatchMode::Channel));
        assert_eq!(config.queries[1].dispatch_mode, None);
    }
}
