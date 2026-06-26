// Copyright 2026 The Drasi Authors.
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

use super::*;
use crate::descriptor::{
    AggregationModeDto, DashboardReactionConfigDto, DashboardReactionDescriptor,
    DashboardWidgetDto, GridOptionsDto, PredefinedDashboardDto, WidgetGridDto, WidgetTypeDto,
};
use drasi_lib::Reaction;
use drasi_plugin_sdk::prelude::ReactionPluginDescriptor;

// -----------------------------------------------------------------------
// Builder tests (existing)
// -----------------------------------------------------------------------

#[test]
fn test_dashboard_builder_defaults() {
    let reaction = DashboardReactionBuilder::new("dashboard-test")
        .build()
        .expect("builder should succeed");
    assert_eq!(reaction.id(), "dashboard-test");
    assert_eq!(reaction.type_name(), "dashboard");

    let properties = reaction.properties();
    assert_eq!(
        properties.get("host"),
        Some(&serde_json::Value::String("0.0.0.0".to_string()))
    );
    assert_eq!(
        properties.get("port"),
        Some(&serde_json::Value::Number(3000_u16.into()))
    );
    assert_eq!(
        properties.get("heartbeatIntervalMs"),
        Some(&serde_json::Value::Number(30_000_u64.into()))
    );
}

#[test]
fn test_dashboard_builder_custom_values() {
    let reaction = DashboardReaction::builder("dashboard-custom")
        .with_query("q1")
        .with_host("127.0.0.1")
        .with_port(18000)
        .with_heartbeat_interval_ms(5000)
        .with_priority_queue_capacity(500)
        .with_auto_start(false)
        .build()
        .expect("builder should succeed");

    assert_eq!(reaction.id(), "dashboard-custom");
    assert_eq!(reaction.query_ids(), vec!["q1".to_string()]);
    assert!(!reaction.auto_start());

    let properties = reaction.properties();
    assert_eq!(
        properties.get("host"),
        Some(&serde_json::Value::String("127.0.0.1".to_string()))
    );
    assert_eq!(
        properties.get("port"),
        Some(&serde_json::Value::Number(18_000_u16.into()))
    );
}

#[test]
fn test_dashboard_config_serialization() {
    let config = DashboardReactionConfig {
        host: "localhost".to_string(),
        port: 5050,
        heartbeat_interval_ms: 15_000,
    };

    let serialized = serde_json::to_string(&config).expect("config should serialize");
    let deserialized: DashboardReactionConfig =
        serde_json::from_str(&serialized).expect("config should deserialize");
    assert_eq!(config, deserialized);
}

// -----------------------------------------------------------------------
// Descriptor metadata tests
// -----------------------------------------------------------------------

#[test]
fn test_descriptor_kind() {
    let desc = DashboardReactionDescriptor;
    assert_eq!(desc.kind(), "dashboard");
}

#[test]
fn test_descriptor_config_version() {
    let desc = DashboardReactionDescriptor;
    assert_eq!(desc.config_version(), "1.0.0");
}

#[test]
fn test_descriptor_config_schema_name() {
    let desc = DashboardReactionDescriptor;
    assert_eq!(
        desc.config_schema_name(),
        "reaction.dashboard.DashboardReactionConfig"
    );
}

// -----------------------------------------------------------------------
// Schema completeness tests
// -----------------------------------------------------------------------

#[test]
fn test_schema_json_is_valid_json() {
    let desc = DashboardReactionDescriptor;
    let schema_json = desc.config_schema_json();
    let parsed: serde_json::Value =
        serde_json::from_str(&schema_json).expect("schema JSON must be valid");
    assert!(parsed.is_object(), "schema must be a JSON object");
}

#[test]
fn test_schema_contains_all_dto_types() {
    let desc = DashboardReactionDescriptor;
    let schema_json = desc.config_schema_json();
    let schemas: serde_json::Value = serde_json::from_str(&schema_json).unwrap();

    let expected_types = [
        "reaction.dashboard.DashboardReactionConfig",
        "reaction.dashboard.GridOptions",
        "reaction.dashboard.WidgetGrid",
        "reaction.dashboard.DashboardWidget",
        "reaction.dashboard.PredefinedDashboard",
        "reaction.dashboard.WidgetType",
        "reaction.dashboard.AggregationMode",
    ];

    for type_name in &expected_types {
        assert!(
            schemas.get(type_name).is_some(),
            "schema must contain type '{type_name}'"
        );
    }
}

#[test]
fn test_schema_config_has_all_properties() {
    let desc = DashboardReactionDescriptor;
    let schema_json = desc.config_schema_json();
    let schemas: serde_json::Value = serde_json::from_str(&schema_json).unwrap();

    let config_schema = &schemas["reaction.dashboard.DashboardReactionConfig"];
    let properties = config_schema["properties"]
        .as_object()
        .expect("config schema must have properties");

    let expected_fields = [
        "host",
        "port",
        "heartbeatIntervalMs",
        "priorityQueueCapacity",
        "predefinedDashboards",
    ];

    for field in &expected_fields {
        assert!(
            properties.contains_key(*field),
            "config schema must contain property '{field}'"
        );
    }
}

#[test]
fn test_schema_predefined_dashboard_has_all_properties() {
    let desc = DashboardReactionDescriptor;
    let schema_json = desc.config_schema_json();
    let schemas: serde_json::Value = serde_json::from_str(&schema_json).unwrap();

    let dashboard_schema = &schemas["reaction.dashboard.PredefinedDashboard"];
    let properties = dashboard_schema["properties"]
        .as_object()
        .expect("PredefinedDashboard schema must have properties");

    for field in &["id", "name", "gridOptions", "widgets"] {
        assert!(
            properties.contains_key(*field),
            "PredefinedDashboard schema must contain '{field}'"
        );
    }
}

#[test]
fn test_schema_widget_has_all_properties() {
    let desc = DashboardReactionDescriptor;
    let schema_json = desc.config_schema_json();
    let schemas: serde_json::Value = serde_json::from_str(&schema_json).unwrap();

    let widget_schema = &schemas["reaction.dashboard.DashboardWidget"];
    let properties = widget_schema["properties"]
        .as_object()
        .expect("DashboardWidget schema must have properties");

    for field in &["id", "type", "title", "grid", "config"] {
        assert!(
            properties.contains_key(*field),
            "DashboardWidget schema must contain '{field}'"
        );
    }
}

// -----------------------------------------------------------------------
// DTO deserialization tests
// -----------------------------------------------------------------------

#[test]
fn test_dto_deserialize_minimal_config() {
    let json = serde_json::json!({});
    let dto: DashboardReactionConfigDto =
        serde_json::from_value(json).expect("empty config should deserialize");
    assert!(dto.host.is_none());
    assert!(dto.port.is_none());
    assert!(dto.heartbeat_interval_ms.is_none());
    assert!(dto.priority_queue_capacity.is_none());
    assert!(dto.predefined_dashboards.is_empty());
}

#[test]
fn test_dto_deserialize_full_config() {
    let json = serde_json::json!({
        "host": "127.0.0.1",
        "port": 8080,
        "heartbeatIntervalMs": 5000,
        "resultsApiUrl": "http://localhost:9000",
        "priorityQueueCapacity": 2048,
        "predefinedDashboards": [
            {
                "id": "test-dash",
                "name": "Test Dashboard",
                "gridOptions": {
                    "columns": 16,
                    "rowHeight": 80,
                    "margin": 5
                },
                "widgets": [
                    {
                        "id": "w1",
                        "type": "kpi",
                        "title": "Total Count",
                        "grid": { "x": 0, "y": 0, "w": 4, "h": 2 },
                        "config": { "queryId": "q1", "valueField": "count", "aggregation": "sum" }
                    },
                    {
                        "id": "w2",
                        "type": "table",
                        "title": "All Data",
                        "config": { "queryId": "q2" }
                    }
                ]
            }
        ]
    });

    let dto: DashboardReactionConfigDto =
        serde_json::from_value(json).expect("full config should deserialize");

    assert!(dto.host.is_some());
    assert!(dto.port.is_some());
    assert!(dto.heartbeat_interval_ms.is_some());
    assert!(dto.priority_queue_capacity.is_some());
    assert_eq!(dto.predefined_dashboards.len(), 1);

    let dash = &dto.predefined_dashboards[0];
    assert_eq!(dash.id, "test-dash");
    assert_eq!(dash.name, "Test Dashboard");
    assert_eq!(dash.widgets.len(), 2);

    let grid_opts = dash.grid_options.as_ref().unwrap();
    assert_eq!(grid_opts.columns, 16);
    assert_eq!(grid_opts.row_height, 80);
    assert_eq!(grid_opts.margin, 5);

    let w1 = &dash.widgets[0];
    assert_eq!(w1.id, "w1");
    assert_eq!(w1.widget_type, WidgetTypeDto::Kpi);
    assert_eq!(w1.title, "Total Count");
    let grid = w1.grid.as_ref().unwrap();
    assert_eq!((grid.x, grid.y, grid.w, grid.h), (0, 0, 4, 2));

    let w2 = &dash.widgets[1];
    assert_eq!(w2.id, "w2");
    assert_eq!(w2.widget_type, WidgetTypeDto::Table);
    assert!(w2.grid.is_none());
}

#[test]
fn test_dto_deserialize_grid_options_defaults() {
    let json = serde_json::json!({});
    let dto: GridOptionsDto =
        serde_json::from_value(json).expect("empty grid options should use defaults");
    assert_eq!(dto.columns, 12);
    assert_eq!(dto.row_height, 60);
    assert_eq!(dto.margin, 10);
}

#[test]
fn test_dto_deserialize_predefined_dashboard_without_grid_options() {
    let json = serde_json::json!({
        "id": "no-grid",
        "name": "No Grid Dashboard"
    });
    let dto: PredefinedDashboardDto =
        serde_json::from_value(json).expect("dashboard without grid should deserialize");
    assert!(dto.grid_options.is_none());
    assert!(dto.widgets.is_empty());
}

// -----------------------------------------------------------------------
// DTO → domain model mapping tests
// -----------------------------------------------------------------------

#[test]
fn test_map_grid_options() {
    use crate::descriptor::GridOptionsDto;
    let dto = GridOptionsDto {
        columns: 16,
        row_height: 80,
        margin: 5,
    };
    let domain = crate::descriptor::map_grid_options(&dto);
    assert_eq!(domain.columns, 16);
    assert_eq!(domain.row_height, 80);
    assert_eq!(domain.margin, 5);
}

#[test]
fn test_map_widget_with_grid() {
    let dto = DashboardWidgetDto {
        id: "w-test".into(),
        widget_type: WidgetTypeDto::Gauge,
        title: "Max Temp".into(),
        grid: Some(WidgetGridDto {
            x: 4,
            y: 2,
            w: 6,
            h: 3,
        }),
        config: serde_json::json!({ "queryId": "q1", "valueField": "temp" }),
    };
    let domain = crate::descriptor::map_widget(&dto);
    assert_eq!(domain.id, "w-test");
    assert_eq!(domain.widget_type, "gauge");
    assert_eq!(domain.title, "Max Temp");
    assert_eq!(domain.grid.x, 4);
    assert_eq!(domain.grid.y, 2);
    assert_eq!(domain.grid.w, 6);
    assert_eq!(domain.grid.h, 3);
    assert_eq!(domain.config["queryId"], "q1");
}

#[test]
fn test_map_widget_without_grid_uses_default() {
    let dto = DashboardWidgetDto {
        id: "w-no-grid".into(),
        widget_type: WidgetTypeDto::Table,
        title: "Data".into(),
        grid: None,
        config: serde_json::json!({}),
    };
    let domain = crate::descriptor::map_widget(&dto);
    assert_eq!(domain.grid.x, 0);
    assert_eq!(domain.grid.y, 0);
    assert_eq!(domain.grid.w, 4);
    assert_eq!(domain.grid.h, 3);
}

#[test]
fn test_map_predefined_dashboard() {
    let dto = PredefinedDashboardDto {
        id: "test-id".into(),
        name: "Test Dashboard".into(),
        grid_options: Some(GridOptionsDto {
            columns: 8,
            row_height: 50,
            margin: 15,
        }),
        widgets: vec![DashboardWidgetDto {
            id: "w1".into(),
            widget_type: WidgetTypeDto::Kpi,
            title: "Count".into(),
            grid: Some(WidgetGridDto {
                x: 0,
                y: 0,
                w: 3,
                h: 2,
            }),
            config: serde_json::json!({ "queryId": "q1" }),
        }],
    };
    let domain = crate::descriptor::map_predefined_dashboard(&dto);
    assert_eq!(domain.id, "test-id");
    assert_eq!(domain.name, "Test Dashboard");
    assert_eq!(domain.grid_options.columns, 8);
    assert_eq!(domain.grid_options.row_height, 50);
    assert_eq!(domain.widgets.len(), 1);
    assert_eq!(domain.widgets[0].id, "w1");
}

#[test]
fn test_map_predefined_dashboard_without_grid_uses_default() {
    let dto = PredefinedDashboardDto {
        id: "default-grid".into(),
        name: "Default Grid".into(),
        grid_options: None,
        widgets: vec![],
    };
    let domain = crate::descriptor::map_predefined_dashboard(&dto);
    assert_eq!(domain.grid_options.columns, 12);
    assert_eq!(domain.grid_options.row_height, 60);
    assert_eq!(domain.grid_options.margin, 10);
}

// -----------------------------------------------------------------------
// create_reaction integration tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_create_reaction_minimal_config() {
    let desc = DashboardReactionDescriptor;
    let config = serde_json::json!({});

    let reaction = desc
        .create_reaction("test-minimal", vec!["q1".into()], &config, true)
        .await
        .expect("create_reaction with empty config should succeed");

    assert_eq!(reaction.id(), "test-minimal");
    assert_eq!(reaction.query_ids(), vec!["q1".to_string()]);
    assert!(reaction.auto_start());

    let props = reaction.properties();
    // With raw_config set, properties() returns the original JSON as-is.
    // Empty config means no keys (defaults are internal, not persisted).
    assert!(props.is_empty());
}

#[tokio::test]
async fn test_create_reaction_full_config() {
    let desc = DashboardReactionDescriptor;
    let config = serde_json::json!({
        "host": "127.0.0.1",
        "port": 9090,
        "heartbeatIntervalMs": 10000,
        "resultsApiUrl": "http://localhost:8080",
        "priorityQueueCapacity": 4096,
        "predefinedDashboards": [
            {
                "id": "pd-1",
                "name": "Predefined One",
                "widgets": [
                    {
                        "id": "w1",
                        "type": "kpi",
                        "title": "Count",
                        "grid": { "x": 0, "y": 0, "w": 4, "h": 2 },
                        "config": { "queryId": "q1", "valueField": "count" }
                    }
                ]
            }
        ]
    });

    let reaction = desc
        .create_reaction("test-full", vec!["q1".into(), "q2".into()], &config, false)
        .await
        .expect("create_reaction with full config should succeed");

    assert_eq!(reaction.id(), "test-full");
    assert_eq!(
        reaction.query_ids(),
        vec!["q1".to_string(), "q2".to_string()]
    );
    assert!(!reaction.auto_start());

    let props = reaction.properties();
    // raw_config preserves original camelCase keys and values
    assert_eq!(props["host"], "127.0.0.1");
    assert_eq!(props["port"], 9090);
    assert_eq!(props["heartbeatIntervalMs"], 10000);
    assert_eq!(props["resultsApiUrl"], "http://localhost:8080");
}

#[tokio::test]
async fn test_create_reaction_invalid_config_returns_error() {
    let desc = DashboardReactionDescriptor;
    let config = serde_json::json!({
        "port": "not-a-number"
    });

    let result = desc
        .create_reaction("test-invalid", vec![], &config, true)
        .await;

    assert!(result.is_err(), "invalid config should return an error");
}

#[tokio::test]
async fn test_create_reaction_with_env_var_config_value() {
    // ConfigValue supports ${VAR:-default} syntax
    let desc = DashboardReactionDescriptor;
    let config = serde_json::json!({
        "host": "${DASHBOARD_HOST:-0.0.0.0}",
        "port": "${DASHBOARD_PORT:-4000}"
    });

    let reaction = desc
        .create_reaction("test-env", vec![], &config, true)
        .await
        .expect("env var config values with defaults should resolve");

    let props = reaction.properties();
    // raw_config preserves original unresolved ConfigValue strings
    assert_eq!(props["host"], "${DASHBOARD_HOST:-0.0.0.0}");
    assert_eq!(props["port"], "${DASHBOARD_PORT:-4000}");
}

// -----------------------------------------------------------------------
// Enum serialization / deserialization tests
// -----------------------------------------------------------------------

#[test]
fn test_widget_type_enum_serializes_to_snake_case() {
    let cases = [
        (WidgetTypeDto::LineChart, "line_chart"),
        (WidgetTypeDto::BarChart, "bar_chart"),
        (WidgetTypeDto::PieChart, "pie_chart"),
        (WidgetTypeDto::Table, "table"),
        (WidgetTypeDto::Gauge, "gauge"),
        (WidgetTypeDto::Kpi, "kpi"),
        (WidgetTypeDto::Text, "text"),
        (WidgetTypeDto::Map, "map"),
    ];

    for (variant, expected) in &cases {
        let json = serde_json::to_value(variant).unwrap();
        assert_eq!(json.as_str().unwrap(), *expected, "WidgetType::{variant:?}");
    }
}

#[test]
fn test_widget_type_enum_deserializes_from_snake_case() {
    for name in &[
        "line_chart",
        "bar_chart",
        "pie_chart",
        "table",
        "gauge",
        "kpi",
        "text",
        "map",
    ] {
        let json = serde_json::Value::String(name.to_string());
        let result: Result<WidgetTypeDto, _> = serde_json::from_value(json);
        assert!(result.is_ok(), "should deserialize '{name}'");
    }
}

#[test]
fn test_widget_type_rejects_invalid_value() {
    let json = serde_json::Value::String("invalid_widget".into());
    let result: Result<WidgetTypeDto, _> = serde_json::from_value(json);
    assert!(result.is_err(), "invalid widget type should fail");
}

#[test]
fn test_aggregation_mode_enum_serializes_to_snake_case() {
    let cases = [
        (AggregationModeDto::Last, "last"),
        (AggregationModeDto::First, "first"),
        (AggregationModeDto::Sum, "sum"),
        (AggregationModeDto::Avg, "avg"),
        (AggregationModeDto::Count, "count"),
        (AggregationModeDto::Min, "min"),
        (AggregationModeDto::Max, "max"),
        (AggregationModeDto::Filter, "filter"),
    ];

    for (variant, expected) in &cases {
        let json = serde_json::to_value(variant).unwrap();
        assert_eq!(
            json.as_str().unwrap(),
            *expected,
            "AggregationMode::{variant:?}"
        );
    }
}

#[test]
fn test_aggregation_mode_rejects_invalid_value() {
    let json = serde_json::Value::String("median".into());
    let result: Result<AggregationModeDto, _> = serde_json::from_value(json);
    assert!(result.is_err(), "invalid aggregation mode should fail");
}

#[test]
fn test_map_widget_preserves_enum_as_snake_case_string() {
    let dto = DashboardWidgetDto {
        id: "w-enum".into(),
        widget_type: WidgetTypeDto::BarChart,
        title: "Test".into(),
        grid: None,
        config: serde_json::json!({}),
    };
    let domain = crate::descriptor::map_widget(&dto);
    assert_eq!(domain.widget_type, "bar_chart");
}

#[test]
fn test_schema_widget_type_is_enum_with_all_variants() {
    let desc = DashboardReactionDescriptor;
    let schema_json = desc.config_schema_json();
    let schemas: serde_json::Value = serde_json::from_str(&schema_json).unwrap();

    let widget_type_schema = &schemas["reaction.dashboard.WidgetType"];
    let enum_values = widget_type_schema["enum"]
        .as_array()
        .expect("WidgetType schema must have enum array");

    let expected = [
        "line_chart",
        "bar_chart",
        "pie_chart",
        "table",
        "gauge",
        "kpi",
        "text",
        "map",
    ];
    let actual: Vec<&str> = enum_values.iter().filter_map(|v| v.as_str()).collect();
    for e in &expected {
        assert!(actual.contains(e), "WidgetType enum must contain '{e}'");
    }
}

#[test]
fn test_schema_aggregation_mode_is_enum_with_all_variants() {
    let desc = DashboardReactionDescriptor;
    let schema_json = desc.config_schema_json();
    let schemas: serde_json::Value = serde_json::from_str(&schema_json).unwrap();

    let agg_schema = &schemas["reaction.dashboard.AggregationMode"];
    let enum_values = agg_schema["enum"]
        .as_array()
        .expect("AggregationMode schema must have enum array");

    let expected = [
        "last", "first", "sum", "avg", "count", "min", "max", "filter",
    ];
    let actual: Vec<&str> = enum_values.iter().filter_map(|v| v.as_str()).collect();
    for e in &expected {
        assert!(
            actual.contains(e),
            "AggregationMode enum must contain '{e}'"
        );
    }
}

// -----------------------------------------------------------------------
// Snapshot store tests — verifies rows vs aggregation separation
// -----------------------------------------------------------------------

use crate::websocket::QuerySnapshotStore;
use chrono::Utc;
use drasi_lib::channels::{QueryResult, ResultDiff};
use std::collections::HashMap;

fn make_query_result(query_id: &str, diffs: Vec<ResultDiff>) -> QueryResult {
    QueryResult {
        query_id: query_id.to_string(),
        sequence: 0,
        timestamp: Utc::now(),
        results: diffs,
        metadata: HashMap::new(),
        profiling: None,
    }
}

#[tokio::test]
async fn test_snapshot_store_add_rows() {
    let store = QuerySnapshotStore::new();
    let qr = make_query_result(
        "q1",
        vec![
            ResultDiff::Add {
                data: serde_json::json!({"id": 1, "name": "Alice"}),
                row_signature: 0,
            },
            ResultDiff::Add {
                data: serde_json::json!({"id": 2, "name": "Bob"}),
                row_signature: 0,
            },
        ],
    );
    store.apply(&qr).await;
    let snapshot = store.get_snapshot("q1").await;
    assert_eq!(snapshot.rows.len(), 2);
    assert!(snapshot.aggregation.is_none());
}

#[tokio::test]
async fn test_snapshot_store_aggregation_separated_from_rows() {
    let store = QuerySnapshotStore::new();
    let qr = make_query_result(
        "q1",
        vec![
            ResultDiff::Add {
                data: serde_json::json!({"id": 1, "temp": 22.5}),
                row_signature: 0,
            },
            ResultDiff::Aggregation {
                before: None,
                after: serde_json::json!({"avg_temp": 22.5, "count": 1}),
                row_signature: 0,
            },
        ],
    );
    store.apply(&qr).await;
    let snapshot = store.get_snapshot("q1").await;
    assert_eq!(
        snapshot.rows.len(),
        1,
        "aggregation must not appear in rows"
    );
    assert_eq!(
        snapshot.aggregation,
        Some(serde_json::json!({"avg_temp": 22.5, "count": 1}))
    );
}

#[tokio::test]
async fn test_snapshot_store_aggregation_update_replaces() {
    let store = QuerySnapshotStore::new();
    store
        .apply(&make_query_result(
            "q1",
            vec![ResultDiff::Aggregation {
                before: None,
                after: serde_json::json!({"count": 1}),
                row_signature: 0,
            }],
        ))
        .await;
    store
        .apply(&make_query_result(
            "q1",
            vec![ResultDiff::Aggregation {
                before: Some(serde_json::json!({"count": 1})),
                after: serde_json::json!({"count": 2}),
                row_signature: 0,
            }],
        ))
        .await;
    let snapshot = store.get_snapshot("q1").await;
    assert!(snapshot.rows.is_empty());
    assert_eq!(snapshot.aggregation, Some(serde_json::json!({"count": 2})));
}

#[tokio::test]
async fn test_snapshot_store_delete_row() {
    let store = QuerySnapshotStore::new();
    store
        .apply(&make_query_result(
            "q1",
            vec![
                ResultDiff::Add {
                    data: serde_json::json!({"id": 1}),
                    row_signature: 0,
                },
                ResultDiff::Add {
                    data: serde_json::json!({"id": 2}),
                    row_signature: 0,
                },
            ],
        ))
        .await;
    store
        .apply(&make_query_result(
            "q1",
            vec![ResultDiff::Delete {
                data: serde_json::json!({"id": 1}),
                row_signature: 0,
            }],
        ))
        .await;
    let snapshot = store.get_snapshot("q1").await;
    assert_eq!(snapshot.rows.len(), 1);
    assert_eq!(snapshot.rows[0].data["id"], 2);
}

#[tokio::test]
async fn test_snapshot_store_update_row() {
    let store = QuerySnapshotStore::new();
    store
        .apply(&make_query_result(
            "q1",
            vec![ResultDiff::Add {
                data: serde_json::json!({"id": 1, "name": "Alice"}),
                row_signature: 0,
            }],
        ))
        .await;
    store
        .apply(&make_query_result(
            "q1",
            vec![ResultDiff::Update {
                data: serde_json::json!({"id": 1, "name": "Alicia"}),
                before: serde_json::json!({"id": 1, "name": "Alice"}),
                after: serde_json::json!({"id": 1, "name": "Alicia"}),
                grouping_keys: None,
                row_signature: 0,
            }],
        ))
        .await;
    let snapshot = store.get_snapshot("q1").await;
    assert_eq!(snapshot.rows.len(), 1);
    assert_eq!(snapshot.rows[0].data["name"], "Alicia");
}

#[tokio::test]
async fn test_snapshot_store_fifo_eviction_rows_only() {
    let store = QuerySnapshotStore::with_max_rows(2);
    store
        .apply(&make_query_result(
            "q1",
            vec![
                ResultDiff::Add {
                    data: serde_json::json!({"id": 1}),
                    row_signature: 0,
                },
                ResultDiff::Add {
                    data: serde_json::json!({"id": 2}),
                    row_signature: 0,
                },
                ResultDiff::Add {
                    data: serde_json::json!({"id": 3}),
                    row_signature: 0,
                },
                ResultDiff::Aggregation {
                    before: None,
                    after: serde_json::json!({"total": 6}),
                    row_signature: 0,
                },
            ],
        ))
        .await;
    let snapshot = store.get_snapshot("q1").await;
    // Should keep only 2 newest rows (evict id:1)
    assert_eq!(snapshot.rows.len(), 2);
    assert_eq!(snapshot.rows[0].data["id"], 2);
    assert_eq!(snapshot.rows[1].data["id"], 3);
    // Aggregation must NOT be evicted
    assert_eq!(snapshot.aggregation, Some(serde_json::json!({"total": 6})));
}

#[tokio::test]
async fn test_snapshot_store_empty_query() {
    let store = QuerySnapshotStore::new();
    let snapshot = store.get_snapshot("nonexistent").await;
    assert!(snapshot.rows.is_empty());
    assert!(snapshot.aggregation.is_none());
}

#[tokio::test]
async fn test_snapshot_store_has_data() {
    let store = QuerySnapshotStore::new();
    assert!(!store.has_data("q1").await);
    store
        .apply(&make_query_result(
            "q1",
            vec![ResultDiff::Aggregation {
                before: None,
                after: serde_json::json!({"count": 1}),
                row_signature: 0,
            }],
        ))
        .await;
    assert!(store.has_data("q1").await);
}

#[tokio::test]
async fn test_snapshot_serialization_format() {
    let store = QuerySnapshotStore::new();
    store
        .apply(&make_query_result(
            "q1",
            vec![
                ResultDiff::Add {
                    data: serde_json::json!({"id": 1}),
                    row_signature: 0,
                },
                ResultDiff::Aggregation {
                    before: None,
                    after: serde_json::json!({"total": 42}),
                    row_signature: 0,
                },
            ],
        ))
        .await;
    let snapshot = store.get_snapshot("q1").await;
    let json = serde_json::to_value(&snapshot).unwrap();
    // Verify the JSON structure the frontend expects: rows are { k, v } envelopes
    // carrying the row_signature so the client can key by identity.
    assert!(json["rows"].is_array());
    assert_eq!(json["rows"].as_array().unwrap().len(), 1);
    assert_eq!(json["rows"][0]["v"]["id"], 1);
    assert_eq!(json["rows"][0]["k"], 0);
    assert_eq!(json["aggregation"]["total"], 42);
}

#[tokio::test]
async fn test_snapshot_store_add_upserts_by_row_signature() {
    // With a real row_signature and no `id` field, an `add` for the same
    // signature must upsert (the canonical identity, independent of `id`).
    let store = QuerySnapshotStore::new();
    store
        .apply(&make_query_result(
            "q1",
            vec![ResultDiff::Add {
                data: serde_json::json!({"location": "Parking"}),
                row_signature: 7777,
            }],
        ))
        .await;
    store
        .apply(&make_query_result(
            "q1",
            vec![ResultDiff::Add {
                data: serde_json::json!({"location": "Curbside"}),
                row_signature: 7777,
            }],
        ))
        .await;
    let snapshot = store.get_snapshot("q1").await;
    assert_eq!(snapshot.rows.len(), 1, "same signature must upsert");
    assert_eq!(snapshot.rows[0].data["location"], "Curbside");
    assert_eq!(snapshot.rows[0].row_signature, 7777);
}

#[tokio::test]
async fn test_snapshot_store_seeded_signature_then_add_upserts() {
    // Seed with a signature, then a live add for the same signature upserts even
    // though the rows carry no `id` column (the #605 plugin scenario).
    let store = QuerySnapshotStore::new();
    store
        .seed_rows("q1", vec![(42, serde_json::json!({"location": "Parking"}))])
        .await;
    store
        .apply(&make_query_result(
            "q1",
            vec![ResultDiff::Add {
                data: serde_json::json!({"location": "Curbside"}),
                row_signature: 42,
            }],
        ))
        .await;
    let snapshot = store.get_snapshot("q1").await;
    assert_eq!(snapshot.rows.len(), 1);
    assert_eq!(snapshot.rows[0].data["location"], "Curbside");
}

#[tokio::test]
async fn test_snapshot_store_distinct_signatures_append() {
    let store = QuerySnapshotStore::new();
    store
        .apply(&make_query_result(
            "q1",
            vec![
                ResultDiff::Add {
                    data: serde_json::json!({"v": 1}),
                    row_signature: 1,
                },
                ResultDiff::Add {
                    data: serde_json::json!({"v": 2}),
                    row_signature: 2,
                },
            ],
        ))
        .await;
    let snapshot = store.get_snapshot("q1").await;
    assert_eq!(snapshot.rows.len(), 2);
}

// -----------------------------------------------------------------------
// Upsert-on-add and bootstrap seeding (issue #605)
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_snapshot_store_add_upserts_existing_id() {
    // An `add` for a row whose `id` already exists must replace it (upsert),
    // not append a duplicate. This is the core of issue #605: the MSSQL source
    // delivers the first post-bootstrap change to an existing row as `op: add`.
    let store = QuerySnapshotStore::new();
    store
        .apply(&make_query_result(
            "q1",
            vec![ResultDiff::Add {
                data: serde_json::json!({"id": "A1234", "location": "Parking"}),
                row_signature: 0,
            }],
        ))
        .await;
    store
        .apply(&make_query_result(
            "q1",
            vec![ResultDiff::Add {
                data: serde_json::json!({"id": "A1234", "location": "Curbside"}),
                row_signature: 0,
            }],
        ))
        .await;

    let snapshot = store.get_snapshot("q1").await;
    assert_eq!(snapshot.rows.len(), 1, "duplicate row was appended");
    assert_eq!(snapshot.rows[0].data["location"], "Curbside");
}

#[tokio::test]
async fn test_snapshot_store_add_appends_distinct_ids() {
    let store = QuerySnapshotStore::new();
    store
        .apply(&make_query_result(
            "q1",
            vec![
                ResultDiff::Add {
                    data: serde_json::json!({"id": "A", "v": 1}),
                    row_signature: 0,
                },
                ResultDiff::Add {
                    data: serde_json::json!({"id": "B", "v": 2}),
                    row_signature: 0,
                },
            ],
        ))
        .await;
    let snapshot = store.get_snapshot("q1").await;
    assert_eq!(snapshot.rows.len(), 2);
}

#[tokio::test]
async fn test_snapshot_store_seed_rows_populates_store() {
    let store = QuerySnapshotStore::new();
    store
        .seed_rows(
            "q1",
            vec![
                (0, serde_json::json!({"id": "A1234", "location": "Parking"})),
                (0, serde_json::json!({"id": "B5678", "location": "Parking"})),
            ],
        )
        .await;

    let snapshot = store.get_snapshot("q1").await;
    assert_eq!(snapshot.rows.len(), 2);
    assert!(store.has_data("q1").await);
}

#[tokio::test]
async fn test_snapshot_store_seeded_row_then_add_upserts() {
    // The end-to-end #605 scenario: bootstrap seeds a row, then the first
    // post-bootstrap change arrives as `op: add` and must replace, not duplicate.
    let store = QuerySnapshotStore::new();
    store
        .seed_rows(
            "q1",
            vec![(0, serde_json::json!({"id": "A1234", "location": "Parking"}))],
        )
        .await;
    store
        .apply(&make_query_result(
            "q1",
            vec![ResultDiff::Add {
                data: serde_json::json!({"id": "A1234", "location": "Curbside"}),
                row_signature: 0,
            }],
        ))
        .await;

    let snapshot = store.get_snapshot("q1").await;
    assert_eq!(snapshot.rows.len(), 1);
    assert_eq!(snapshot.rows[0].data["location"], "Curbside");
}

#[tokio::test]
async fn test_snapshot_store_seed_rows_does_not_overwrite_existing() {
    // Seeding must not clobber newer live data that already arrived for a key.
    let store = QuerySnapshotStore::new();
    store
        .apply(&make_query_result(
            "q1",
            vec![ResultDiff::Add {
                data: serde_json::json!({"id": "A1234", "location": "Curbside"}),
                row_signature: 0,
            }],
        ))
        .await;
    store
        .seed_rows(
            "q1",
            vec![
                (0, serde_json::json!({"id": "A1234", "location": "Parking"})),
                (0, serde_json::json!({"id": "B5678", "location": "Parking"})),
            ],
        )
        .await;

    let snapshot = store.get_snapshot("q1").await;
    assert_eq!(snapshot.rows.len(), 2);
    let a = snapshot
        .rows
        .iter()
        .find(|r| r.data["id"] == "A1234")
        .expect("A1234 present");
    assert_eq!(
        a.data["location"], "Curbside",
        "stale seed overwrote live data"
    );
}
