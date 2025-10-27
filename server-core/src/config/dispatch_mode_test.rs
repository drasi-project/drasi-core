#[cfg(test)]
mod tests {
    use super::super::schema::*;
    use crate::channels::DispatchMode;

    #[test]
    fn test_source_config_with_dispatch_mode() {
        let yaml = r#"
            id: test_source
            source_type: mock
            dispatch_mode: channel
            properties:
                key: value
        "#;

        let config: SourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.id, "test_source");
        assert_eq!(config.dispatch_mode, Some(DispatchMode::Channel));
    }

    #[test]
    fn test_source_config_without_dispatch_mode() {
        let yaml = r#"
            id: test_source
            source_type: mock
            properties:
                key: value
        "#;

        let config: SourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.id, "test_source");
        assert_eq!(config.dispatch_mode, None);
    }

    #[test]
    fn test_query_config_with_dispatch_mode() {
        let yaml = r#"
            id: test_query
            query: "RETURN 1"
            sources: ["source1"]
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
            sources: ["source1"]
        "#;

        let config: QueryConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.id, "test_query");
        assert_eq!(config.dispatch_mode, None);
    }

    #[test]
    fn test_full_config_with_mixed_dispatch_modes() {
        let yaml = r#"
            server_core:
                id: test_server
            sources:
                - id: broadcast_source
                  source_type: mock
                  dispatch_mode: broadcast
                  properties: {}
                - id: channel_source
                  source_type: mock
                  dispatch_mode: channel
                  properties: {}
                - id: default_source
                  source_type: mock
                  properties: {}
            queries:
                - id: query1
                  query: "RETURN 1"
                  sources: ["broadcast_source"]
                  dispatch_mode: channel
                - id: query2
                  query: "RETURN 2"
                  sources: ["channel_source"]
            reactions:
                - id: reaction1
                  reaction_type: log
                  queries: ["query1"]
        "#;

        let config: DrasiServerCoreConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.sources.len(), 3);
        assert_eq!(config.sources[0].dispatch_mode, Some(DispatchMode::Broadcast));
        assert_eq!(config.sources[1].dispatch_mode, Some(DispatchMode::Channel));
        assert_eq!(config.sources[2].dispatch_mode, None);

        assert_eq!(config.queries.len(), 2);
        assert_eq!(config.queries[0].dispatch_mode, Some(DispatchMode::Channel));
        assert_eq!(config.queries[1].dispatch_mode, None);
    }
}