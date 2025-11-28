#[cfg(test)]
mod tests {
    use super::super::schema::*;
    use crate::channels::DispatchMode;

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
    fn test_query_config_with_channel_dispatch_mode() {
        let yaml = r#"
            id: test_query
            query: "RETURN 1"
            source_subscriptions:
              - source_id: source1
                pipeline: []
            dispatch_mode: channel
        "#;

        let config: QueryConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.id, "test_query");
        assert_eq!(config.dispatch_mode, Some(DispatchMode::Channel));
    }

    #[test]
    fn test_full_config_with_mixed_query_dispatch_modes() {
        let mut config = DrasiLibConfig::default();

        // Note: Sources and reactions are now instance-based and not part of config.
        // They are passed as pre-built Arc<dyn Source> and Arc<dyn Reaction> instances.

        // Add queries with different dispatch modes
        config.queries.push(QueryConfig {
            id: "query1".to_string(),
            query: "RETURN 1".to_string(),
            query_language: crate::config::QueryLanguage::Cypher,
            middleware: vec![],
            source_subscriptions: vec![crate::config::SourceSubscriptionConfig {
                source_id: "source1".to_string(),
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
                source_id: "source2".to_string(),
                pipeline: vec![],
            }],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: Some(DispatchMode::Broadcast),
            storage_backend: None,
        });

        config.queries.push(QueryConfig {
            id: "query3".to_string(),
            query: "RETURN 3".to_string(),
            query_language: crate::config::QueryLanguage::Cypher,
            middleware: vec![],
            source_subscriptions: vec![crate::config::SourceSubscriptionConfig {
                source_id: "source3".to_string(),
                pipeline: vec![],
            }],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None, // Default
            storage_backend: None,
        });

        assert_eq!(config.queries.len(), 3);
        assert_eq!(config.queries[0].dispatch_mode, Some(DispatchMode::Channel));
        assert_eq!(
            config.queries[1].dispatch_mode,
            Some(DispatchMode::Broadcast)
        );
        assert_eq!(config.queries[2].dispatch_mode, None);
    }
}
