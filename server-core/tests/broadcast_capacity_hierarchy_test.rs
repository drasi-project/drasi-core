// Copyright 2025 The Drasi Authors.
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

//! Integration tests for broadcast channel capacity three-level hierarchy:
//! 1. Component-specific override (highest priority)
//! 2. Server global setting
//! 3. Hardcoded default (1000)

use drasi_server_core::{
    DrasiServerCoreConfig, DrasiServerCoreSettings, QueryConfig, QueryLanguage, RuntimeConfig,
    SourceConfig,
};
use std::collections::HashMap;

/// Test that without any config, all use hardcoded default of 1000
#[tokio::test]
async fn test_broadcast_capacity_hierarchy_all_defaults() {
    let config = DrasiServerCoreConfig {
        server_core: DrasiServerCoreSettings {
            id: "test-server".to_string(),
            priority_queue_capacity: None,
            broadcast_channel_capacity: None, // No global override
        },
        sources: vec![
            SourceConfig {
                id: "source1".to_string(),
                source_type: "mock".to_string(),
                auto_start: true,
                properties: HashMap::new(),
                bootstrap_provider: None,
                broadcast_channel_capacity: None, // No component override,
                dispatch_mode: None,
            },
            SourceConfig {
                id: "source2".to_string(),
                source_type: "mock".to_string(),
                auto_start: true,
                properties: HashMap::new(),
                bootstrap_provider: None,
                broadcast_channel_capacity: None, // No component override,
                dispatch_mode: None,
            },
        ],
        queries: vec![
            QueryConfig {
                id: "query1".to_string(),
                query: "MATCH (n) RETURN n".to_string(),
                query_language: QueryLanguage::Cypher,
                sources: vec!["source1".to_string()],
                auto_start: true,
                properties: HashMap::new(),
                joins: None,
                enable_bootstrap: true,
                bootstrap_buffer_size: 10000,
                priority_queue_capacity: None,
                broadcast_channel_capacity: None, // No component override,
                dispatch_mode: None,
            },
            QueryConfig {
                id: "query2".to_string(),
                query: "MATCH (m) RETURN m".to_string(),
                query_language: QueryLanguage::Cypher,
                sources: vec!["source2".to_string()],
                auto_start: true,
                properties: HashMap::new(),
                joins: None,
                enable_bootstrap: true,
                bootstrap_buffer_size: 10000,
                priority_queue_capacity: None,
                broadcast_channel_capacity: None, // No component override,
                dispatch_mode: None,
            },
        ],
        reactions: vec![],
    };

    let runtime_config = RuntimeConfig::from(config);

    // All sources should use the hardcoded default of 1000
    assert_eq!(
        runtime_config.sources[0].broadcast_channel_capacity,
        Some(1000),
        "Source 1 should use hardcoded default"
    );
    assert_eq!(
        runtime_config.sources[1].broadcast_channel_capacity,
        Some(1000),
        "Source 2 should use hardcoded default"
    );

    // All queries should use the hardcoded default of 1000
    assert_eq!(
        runtime_config.queries[0].broadcast_channel_capacity,
        Some(1000),
        "Query 1 should use hardcoded default"
    );
    assert_eq!(
        runtime_config.queries[1].broadcast_channel_capacity,
        Some(1000),
        "Query 2 should use hardcoded default"
    );
}

/// Test that global setting applies to all components without overrides
#[tokio::test]
async fn test_broadcast_capacity_hierarchy_global_override() {
    let config = DrasiServerCoreConfig {
        server_core: DrasiServerCoreSettings {
            id: "test-server".to_string(),
            priority_queue_capacity: None,
            broadcast_channel_capacity: Some(5000), // Global override
        },
        sources: vec![
            SourceConfig {
                id: "source1".to_string(),
                source_type: "mock".to_string(),
                auto_start: true,
                properties: HashMap::new(),
                bootstrap_provider: None,
                broadcast_channel_capacity: None, // No component override,
                dispatch_mode: None,
            },
            SourceConfig {
                id: "source2".to_string(),
                source_type: "postgres".to_string(),
                auto_start: true,
                properties: HashMap::new(),
                bootstrap_provider: None,
                broadcast_channel_capacity: None, // No component override,
                dispatch_mode: None,
            },
        ],
        queries: vec![QueryConfig {
            id: "query1".to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            sources: vec!["source1".to_string()],
            auto_start: true,
            properties: HashMap::new(),
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            broadcast_channel_capacity: None, // No component override,
                dispatch_mode: None,        }],
        reactions: vec![],
    };

    let runtime_config = RuntimeConfig::from(config);

    // All sources should use the global override of 5000
    assert_eq!(
        runtime_config.sources[0].broadcast_channel_capacity,
        Some(5000),
        "Source 1 should use global override"
    );
    assert_eq!(
        runtime_config.sources[1].broadcast_channel_capacity,
        Some(5000),
        "Source 2 should use global override"
    );

    // All queries should use the global override of 5000
    assert_eq!(
        runtime_config.queries[0].broadcast_channel_capacity,
        Some(5000),
        "Query 1 should use global override"
    );
}

/// Test that component overrides take precedence over global setting
#[tokio::test]
async fn test_broadcast_capacity_hierarchy_component_override() {
    let config = DrasiServerCoreConfig {
        server_core: DrasiServerCoreSettings {
            id: "test-server".to_string(),
            priority_queue_capacity: None,
            broadcast_channel_capacity: Some(2000), // Global override
        },
        sources: vec![
            SourceConfig {
                id: "source1".to_string(),
                source_type: "mock".to_string(),
                auto_start: true,
                properties: HashMap::new(),
                bootstrap_provider: None,
                broadcast_channel_capacity: Some(10000), // Component override,
                dispatch_mode: None,
            },
            SourceConfig {
                id: "source2".to_string(),
                source_type: "http".to_string(),
                auto_start: true,
                properties: HashMap::new(),
                bootstrap_provider: None,
                broadcast_channel_capacity: None, // No component override,
                dispatch_mode: None,
            },
        ],
        queries: vec![
            QueryConfig {
                id: "query1".to_string(),
                query: "MATCH (n) RETURN n".to_string(),
                query_language: QueryLanguage::Cypher,
                sources: vec!["source1".to_string()],
                auto_start: true,
                properties: HashMap::new(),
                joins: None,
                enable_bootstrap: true,
                bootstrap_buffer_size: 10000,
                priority_queue_capacity: None,
                broadcast_channel_capacity: Some(8000), // Component override,
                dispatch_mode: None,
            },
            QueryConfig {
                id: "query2".to_string(),
                query: "MATCH (m) RETURN m".to_string(),
                query_language: QueryLanguage::Cypher,
                sources: vec!["source2".to_string()],
                auto_start: true,
                properties: HashMap::new(),
                joins: None,
                enable_bootstrap: true,
                bootstrap_buffer_size: 10000,
                priority_queue_capacity: None,
                broadcast_channel_capacity: None, // No component override,
                dispatch_mode: None,
            },
        ],
        reactions: vec![],
    };

    let runtime_config = RuntimeConfig::from(config);

    // Source 1 should use its component override
    assert_eq!(
        runtime_config.sources[0].broadcast_channel_capacity,
        Some(10000),
        "Source 1 should use component override"
    );

    // Source 2 should use the global override
    assert_eq!(
        runtime_config.sources[1].broadcast_channel_capacity,
        Some(2000),
        "Source 2 should use global override"
    );

    // Query 1 should use its component override
    assert_eq!(
        runtime_config.queries[0].broadcast_channel_capacity,
        Some(8000),
        "Query 1 should use component override"
    );

    // Query 2 should use the global override
    assert_eq!(
        runtime_config.queries[1].broadcast_channel_capacity,
        Some(2000),
        "Query 2 should use global override"
    );
}

/// Test mix of all three levels
#[tokio::test]
async fn test_broadcast_capacity_hierarchy_mixed() {
    let config = DrasiServerCoreConfig {
        server_core: DrasiServerCoreSettings {
            id: "test-server".to_string(),
            priority_queue_capacity: Some(50000),
            broadcast_channel_capacity: Some(3000), // Global override
        },
        sources: vec![
            SourceConfig {
                id: "high_volume_source".to_string(),
                source_type: "postgres".to_string(),
                auto_start: true,
                properties: HashMap::new(),
                bootstrap_provider: None,
                broadcast_channel_capacity: Some(20000), // High volume, needs large capacity,
                dispatch_mode: None,
            },
            SourceConfig {
                id: "standard_source".to_string(),
                source_type: "grpc".to_string(),
                auto_start: true,
                properties: HashMap::new(),
                bootstrap_provider: None,
                broadcast_channel_capacity: None, // Uses global (3000),
                dispatch_mode: None,
            },
            SourceConfig {
                id: "low_volume_source".to_string(),
                source_type: "mock".to_string(),
                auto_start: true,
                properties: HashMap::new(),
                bootstrap_provider: None,
                broadcast_channel_capacity: Some(500), // Low volume, small capacity,
                dispatch_mode: None,
            },
        ],
        queries: vec![
            QueryConfig {
                id: "high_fanout_query".to_string(),
                query: "MATCH (n) RETURN n".to_string(),
                query_language: QueryLanguage::Cypher,
                sources: vec!["high_volume_source".to_string()],
                auto_start: true,
                properties: HashMap::new(),
                joins: None,
                enable_bootstrap: true,
                bootstrap_buffer_size: 10000,
                priority_queue_capacity: Some(100000),
                broadcast_channel_capacity: Some(15000), // Many reactions subscribe,
                dispatch_mode: None,
            },
            QueryConfig {
                id: "standard_query".to_string(),
                query: "MATCH (m) RETURN m".to_string(),
                query_language: QueryLanguage::Cypher,
                sources: vec!["standard_source".to_string()],
                auto_start: true,
                properties: HashMap::new(),
                joins: None,
                enable_bootstrap: true,
                bootstrap_buffer_size: 10000,
                priority_queue_capacity: None,    // Uses global (50000)
                broadcast_channel_capacity: None, // Uses global (3000),
                dispatch_mode: None,
            },
        ],
        reactions: vec![],
    };

    let runtime_config = RuntimeConfig::from(config);

    // Verify source capacity hierarchy
    assert_eq!(
        runtime_config.sources[0].broadcast_channel_capacity,
        Some(20000),
        "High volume source should use its override"
    );
    assert_eq!(
        runtime_config.sources[1].broadcast_channel_capacity,
        Some(3000),
        "Standard source should use global"
    );
    assert_eq!(
        runtime_config.sources[2].broadcast_channel_capacity,
        Some(500),
        "Low volume source should use its override"
    );

    // Verify query capacity hierarchy
    assert_eq!(
        runtime_config.queries[0].broadcast_channel_capacity,
        Some(15000),
        "High fanout query should use its override"
    );
    assert_eq!(
        runtime_config.queries[1].broadcast_channel_capacity,
        Some(3000),
        "Standard query should use global"
    );

    // Also verify priority queue capacity is still working
    assert_eq!(
        runtime_config.queries[0].priority_queue_capacity,
        Some(100000),
        "High fanout query priority queue should use its override"
    );
    assert_eq!(
        runtime_config.queries[1].priority_queue_capacity,
        Some(50000),
        "Standard query priority queue should use global"
    );
}

/// Test that no global setting and no component override results in default
#[tokio::test]
async fn test_broadcast_capacity_hierarchy_nil_global_nil_component() {
    let config = DrasiServerCoreConfig {
        server_core: DrasiServerCoreSettings {
            id: "test-server".to_string(),
            priority_queue_capacity: None,
            broadcast_channel_capacity: None, // No global
        },
        sources: vec![SourceConfig {
            id: "source1".to_string(),
            source_type: "platform".to_string(),
            auto_start: true,
            properties: HashMap::new(),
            bootstrap_provider: None,
            broadcast_channel_capacity: None, // No component override,
                dispatch_mode: None,        }],
        queries: vec![QueryConfig {
            id: "query1".to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: drasi_server_core::config::QueryLanguage::Cypher,
            sources: vec!["source1".to_string()],
            auto_start: true,
            properties: HashMap::new(),
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            broadcast_channel_capacity: None, // No component override,
                dispatch_mode: None,        }],
        reactions: vec![],
    };

    let runtime_config = RuntimeConfig::from(config);

    // Should fall back to hardcoded default of 1000
    assert_eq!(
        runtime_config.sources[0].broadcast_channel_capacity,
        Some(1000),
        "Should use hardcoded default when no overrides exist"
    );
    assert_eq!(
        runtime_config.queries[0].broadcast_channel_capacity,
        Some(1000),
        "Should use hardcoded default when no overrides exist"
    );
}
