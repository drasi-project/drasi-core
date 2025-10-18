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

//! Transformation logic for converting drasi-core QueryResult to Platform ChangeEvent

use super::types::{ResultChangeEvent, ResultEvent, UpdatePayload};
use crate::channels::QueryResult;
use crate::profiling::ProfilingMetadata;
use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};

/// Build tracking metadata structure from profiling data
///
/// Converts ProfilingMetadata into the expected metadata.tracking JSON structure
/// for Drasi Platform reactions. The structure includes:
/// - metadata.tracking.source.* - Source timing and sequence information
/// - metadata.tracking.query.* - Query processing timing information
///
/// Field mappings (some fields use the same source value as placeholders):
/// - changeDispatcherEnd_ns, changeDispatcherStart_ns, changeRouterEnd_ns ← source_send_ns
/// - changeRouterStart_ns ← source_receive_ns
/// - reactivatorEnd_ns, reactivatorStart_ns ← source_ns (temporary, will be updated later)
/// - seq ← sequence parameter
/// - source_ns ← source_ns
/// - enqueue_ns ← source_send_ns
/// - dequeue_ns ← query_receive_ns
/// - queryStart_ns ← query_core_call_ns
/// - queryEnd_ns ← query_core_return_ns
fn build_tracking_metadata(profiling: &ProfilingMetadata, sequence: u64) -> Map<String, Value> {
    let mut tracking = Map::new();

    // Build source tracking metadata
    let mut source = Map::new();
    if let Some(source_send_ns) = profiling.source_send_ns {
        source.insert("changeDispatcherEnd_ns".to_string(), json!(source_send_ns));
        source.insert(
            "changeDispatcherStart_ns".to_string(),
            json!(source_send_ns),
        );
        source.insert("changeRouterEnd_ns".to_string(), json!(source_send_ns));
    }
    if let Some(source_receive_ns) = profiling.source_receive_ns {
        source.insert("changeRouterStart_ns".to_string(), json!(source_receive_ns));
    }
    if let Some(source_ns) = profiling.source_ns {
        source.insert("reactivatorEnd_ns".to_string(), json!(source_ns));
        source.insert("reactivatorStart_ns".to_string(), json!(source_ns));
        source.insert("source_ns".to_string(), json!(source_ns));
    }
    source.insert("seq".to_string(), json!(sequence));

    // Build query tracking metadata
    let mut query = Map::new();
    if let Some(source_send_ns) = profiling.source_send_ns {
        query.insert("enqueue_ns".to_string(), json!(source_send_ns));
    }
    if let Some(query_receive_ns) = profiling.query_receive_ns {
        query.insert("dequeue_ns".to_string(), json!(query_receive_ns));
    }
    if let Some(query_core_call_ns) = profiling.query_core_call_ns {
        query.insert("queryStart_ns".to_string(), json!(query_core_call_ns));
    }
    if let Some(query_core_return_ns) = profiling.query_core_return_ns {
        query.insert("queryEnd_ns".to_string(), json!(query_core_return_ns));
    }

    // Only include source/query objects if they have data
    if !source.is_empty() {
        tracking.insert("source".to_string(), Value::Object(source));
    }
    if !query.is_empty() {
        tracking.insert("query".to_string(), Value::Object(query));
    }

    let mut result = Map::new();
    result.insert("tracking".to_string(), Value::Object(tracking));
    result
}

/// Transform a QueryResult into a ResultEvent
pub fn transform_query_result(
    query_result: QueryResult,
    sequence: i64,
    sequence_u64: u64,
) -> Result<ResultEvent> {
    let mut added_results = Vec::new();
    let mut updated_results = Vec::new();
    let mut deleted_results = Vec::new();

    // Extract profiling data early for tracking metadata
    let profiling = query_result.profiling.as_ref();

    // Parse the results array
    for result_item in query_result.results {
        let result_type = result_item
            .get("type")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("Missing 'type' field in result item"))?;

        match result_type {
            "add" | "ADD" => {
                // Extract data field for add operations
                let data = result_item
                    .get("data")
                    .ok_or_else(|| anyhow!("Missing 'data' field in add result"))?
                    .as_object()
                    .ok_or_else(|| anyhow!("'data' field must be an object"))?
                    .clone();
                added_results.push(data);
            }
            "update" | "UPDATE" => {
                // Extract before and after fields for update operations
                let before = result_item
                    .get("before")
                    .ok_or_else(|| anyhow!("Missing 'before' field in update result"))?
                    .as_object()
                    .ok_or_else(|| anyhow!("'before' field must be an object"))?
                    .clone();
                let after = result_item
                    .get("after")
                    .ok_or_else(|| anyhow!("Missing 'after' field in update result"))?
                    .as_object()
                    .ok_or_else(|| anyhow!("'after' field must be an object"))?
                    .clone();

                // Optional grouping keys
                let grouping_keys = result_item
                    .get("grouping_keys")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    });

                updated_results.push(UpdatePayload {
                    before: Some(before),
                    after: Some(after),
                    grouping_keys,
                });
            }
            "delete" | "DELETE" => {
                // Extract data field for delete operations
                let data = result_item
                    .get("data")
                    .ok_or_else(|| anyhow!("Missing 'data' field in delete result"))?
                    .as_object()
                    .ok_or_else(|| anyhow!("'data' field must be an object"))?
                    .clone();
                deleted_results.push(data);
            }
            unknown => {
                log::warn!("Unknown result type: {}, skipping", unknown);
                continue;
            }
        }
    }

    // Convert timestamp to milliseconds since epoch
    let source_time_ms = query_result.timestamp.timestamp_millis() as u64;

    // Filter metadata - remove internal drasi-core fields that shouldn't be exposed
    let mut filtered_metadata: Map<String, Value> = query_result
        .metadata
        .into_iter()
        .filter(|(key, _)| {
            // Filter out internal query/source metadata
            !matches!(
                key.as_str(),
                "query" | "processed_by" | "source_id" | "result_count"
            )
        })
        .collect();

    // Add tracking metadata if profiling data is available
    if let Some(prof) = profiling {
        let tracking_metadata = build_tracking_metadata(prof, sequence_u64);
        // Merge tracking metadata into filtered_metadata
        for (key, value) in tracking_metadata {
            filtered_metadata.insert(key, value);
        }
    }

    // Convert metadata if present (filter out empty metadata)
    let metadata = if filtered_metadata.is_empty() {
        None
    } else {
        Some(filtered_metadata)
    };

    let change_event = ResultChangeEvent {
        query_id: query_result.query_id,
        sequence: sequence as u64,
        source_time_ms,
        added_results,
        updated_results,
        deleted_results,
        metadata,
    };

    Ok(ResultEvent::Change(change_event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn test_transform_add_results() {
        let timestamp = chrono::DateTime::from_timestamp_millis(1609459200000).unwrap();
        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp,
            results: vec![
                json!({
                    "type": "add",
                    "data": {"id": "1", "value": "test1"}
                }),
                json!({
                    "type": "add",
                    "data": {"id": "2", "value": "test2"}
                }),
            ],
            metadata: HashMap::new(),
            profiling: None,
        };

        let result = transform_query_result(query_result, 1, 1).unwrap();

        match result {
            ResultEvent::Change(change) => {
                assert_eq!(change.query_id, "test-query");
                assert_eq!(change.sequence, 1);
                assert_eq!(change.source_time_ms, 1609459200000);
                assert_eq!(change.added_results.len(), 2);
                assert_eq!(change.updated_results.len(), 0);
                assert_eq!(change.deleted_results.len(), 0);
                assert_eq!(change.added_results[0]["id"], "1");
                assert_eq!(change.added_results[1]["id"], "2");
            }
            _ => panic!("Expected Change event"),
        }
    }

    #[test]
    fn test_transform_update_results() {
        let timestamp = chrono::DateTime::from_timestamp_millis(1609459200000).unwrap();
        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp,
            results: vec![json!({
                "type": "update",
                "before": {"id": "1", "value": 10},
                "after": {"id": "1", "value": 20}
            })],
            metadata: HashMap::new(),
            profiling: None,
        };

        let result = transform_query_result(query_result, 2, 2).unwrap();

        match result {
            ResultEvent::Change(change) => {
                assert_eq!(change.updated_results.len(), 1);
                assert_eq!(
                    change.updated_results[0].before.as_ref().unwrap()["value"],
                    10
                );
                assert_eq!(
                    change.updated_results[0].after.as_ref().unwrap()["value"],
                    20
                );
                assert_eq!(change.updated_results[0].grouping_keys, None);
            }
            _ => panic!("Expected Change event"),
        }
    }

    #[test]
    fn test_transform_update_with_grouping_keys() {
        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp: chrono::DateTime::from_timestamp_millis(1609459200000).unwrap(),
            results: vec![json!({
                "type": "update",
                "before": {"id": "1", "value": 10},
                "after": {"id": "1", "value": 20},
                "grouping_keys": ["key1", "key2"]
            })],
            metadata: HashMap::new(),
            profiling: None,
        };

        let result = transform_query_result(query_result, 2, 2).unwrap();

        match result {
            ResultEvent::Change(change) => {
                assert_eq!(change.updated_results.len(), 1);
                assert_eq!(
                    change.updated_results[0].grouping_keys,
                    Some(vec!["key1".to_string(), "key2".to_string()])
                );
            }
            _ => panic!("Expected Change event"),
        }
    }

    #[test]
    fn test_transform_delete_results() {
        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp: chrono::DateTime::from_timestamp_millis(1609459200000).unwrap(),
            results: vec![json!({
                "type": "delete",
                "data": {"id": "1"}
            })],
            metadata: HashMap::new(),
            profiling: None,
        };

        let result = transform_query_result(query_result, 3, 3).unwrap();

        match result {
            ResultEvent::Change(change) => {
                assert_eq!(change.deleted_results.len(), 1);
                assert_eq!(change.deleted_results[0]["id"], "1");
            }
            _ => panic!("Expected Change event"),
        }
    }

    #[test]
    fn test_transform_mixed_results() {
        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp: chrono::DateTime::from_timestamp_millis(1609459200000).unwrap(),
            results: vec![
                json!({"type": "add", "data": {"id": "1"}}),
                json!({
                    "type": "update",
                    "before": {"id": "2", "value": 10},
                    "after": {"id": "2", "value": 20}
                }),
                json!({"type": "delete", "data": {"id": "3"}}),
            ],
            metadata: HashMap::new(),
            profiling: None,
        };

        let result = transform_query_result(query_result, 4, 4).unwrap();

        match result {
            ResultEvent::Change(change) => {
                assert_eq!(change.added_results.len(), 1);
                assert_eq!(change.updated_results.len(), 1);
                assert_eq!(change.deleted_results.len(), 1);
            }
            _ => panic!("Expected Change event"),
        }
    }

    #[test]
    fn test_transform_with_metadata() {
        let mut metadata = HashMap::new();
        metadata.insert("tracking".to_string(), json!({"timing": 100}));
        metadata.insert("query".to_string(), json!({"execution_time": 50})); // Will be filtered out
        metadata.insert("source_id".to_string(), json!("test-source")); // Will be filtered out

        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp: chrono::DateTime::from_timestamp_millis(1609459200000).unwrap(),
            results: vec![json!({"type": "add", "data": {"id": "1"}})],
            metadata,
            profiling: None,
        };

        let result = transform_query_result(query_result, 5, 5).unwrap();

        match result {
            ResultEvent::Change(change) => {
                assert!(change.metadata.is_some());
                let meta = change.metadata.unwrap();
                // Only non-filtered metadata should remain
                assert!(meta.contains_key("tracking"));
                assert!(!meta.contains_key("query")); // Should be filtered
                assert!(!meta.contains_key("source_id")); // Should be filtered
            }
            _ => panic!("Expected Change event"),
        }
    }

    #[test]
    fn test_transform_missing_type_field() {
        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp: chrono::DateTime::from_timestamp_millis(1609459200000).unwrap(),
            results: vec![json!({"data": {"id": "1"}})], // Missing "type" field
            metadata: HashMap::new(),
            profiling: None,
        };

        let result = transform_query_result(query_result, 1, 1);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing 'type' field"));
    }

    #[test]
    fn test_transform_missing_data_field_in_add() {
        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp: chrono::DateTime::from_timestamp_millis(1609459200000).unwrap(),
            results: vec![json!({"type": "add"})], // Missing "data" field
            metadata: HashMap::new(),
            profiling: None,
        };

        let result = transform_query_result(query_result, 1, 1);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing 'data' field"));
    }

    #[test]
    fn test_transform_missing_before_field_in_update() {
        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp: chrono::DateTime::from_timestamp_millis(1609459200000).unwrap(),
            results: vec![json!({
                "type": "update",
                "after": {"id": "1", "value": 20}
            })],
            metadata: HashMap::new(),
            profiling: None,
        };

        let result = transform_query_result(query_result, 1, 1);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing 'before' field"));
    }

    #[test]
    fn test_transform_unknown_type_skipped() {
        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp: chrono::DateTime::from_timestamp_millis(1609459200000).unwrap(),
            results: vec![
                json!({"type": "add", "data": {"id": "1"}}),
                json!({"type": "unknown", "data": {"id": "2"}}), // Unknown type
                json!({"type": "delete", "data": {"id": "3"}}),
            ],
            metadata: HashMap::new(),
            profiling: None,
        };

        let result = transform_query_result(query_result, 1, 1).unwrap();

        match result {
            ResultEvent::Change(change) => {
                assert_eq!(change.added_results.len(), 1);
                assert_eq!(change.deleted_results.len(), 1);
                // Unknown type was skipped
            }
            _ => panic!("Expected Change event"),
        }
    }

    #[test]
    fn test_transform_empty_metadata_filtered() {
        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp: chrono::DateTime::from_timestamp_millis(1609459200000).unwrap(),
            results: vec![json!({"type": "add", "data": {"id": "1"}})],
            metadata: HashMap::new(), // Empty metadata
            profiling: None,
        };

        let result = transform_query_result(query_result, 1, 1).unwrap();

        match result {
            ResultEvent::Change(change) => {
                assert!(change.metadata.is_none());
            }
            _ => panic!("Expected Change event"),
        }
    }

    #[test]
    fn test_build_tracking_metadata_all_fields() {
        use crate::profiling::ProfilingMetadata;

        let profiling = ProfilingMetadata {
            source_ns: Some(1000),
            source_receive_ns: Some(2000),
            source_send_ns: Some(3000),
            query_receive_ns: Some(4000),
            query_core_call_ns: Some(5000),
            query_core_return_ns: Some(6000),
            query_send_ns: Some(7000),
            reaction_receive_ns: Some(8000),
            reaction_complete_ns: Some(9000),
        };

        let result = build_tracking_metadata(&profiling, 42);

        // Verify tracking structure exists
        assert!(result.contains_key("tracking"));
        let tracking = result.get("tracking").unwrap().as_object().unwrap();

        // Verify source tracking
        assert!(tracking.contains_key("source"));
        let source = tracking.get("source").unwrap().as_object().unwrap();
        assert_eq!(
            source.get("changeDispatcherEnd_ns").unwrap().as_u64(),
            Some(3000)
        );
        assert_eq!(
            source.get("changeDispatcherStart_ns").unwrap().as_u64(),
            Some(3000)
        );
        assert_eq!(
            source.get("changeRouterEnd_ns").unwrap().as_u64(),
            Some(3000)
        );
        assert_eq!(
            source.get("changeRouterStart_ns").unwrap().as_u64(),
            Some(2000)
        );
        assert_eq!(
            source.get("reactivatorEnd_ns").unwrap().as_u64(),
            Some(1000)
        );
        assert_eq!(
            source.get("reactivatorStart_ns").unwrap().as_u64(),
            Some(1000)
        );
        assert_eq!(source.get("source_ns").unwrap().as_u64(), Some(1000));
        assert_eq!(source.get("seq").unwrap().as_u64(), Some(42));

        // Verify query tracking
        assert!(tracking.contains_key("query"));
        let query = tracking.get("query").unwrap().as_object().unwrap();
        assert_eq!(query.get("enqueue_ns").unwrap().as_u64(), Some(3000));
        assert_eq!(query.get("dequeue_ns").unwrap().as_u64(), Some(4000));
        assert_eq!(query.get("queryStart_ns").unwrap().as_u64(), Some(5000));
        assert_eq!(query.get("queryEnd_ns").unwrap().as_u64(), Some(6000));
    }

    #[test]
    fn test_build_tracking_metadata_partial_fields() {
        use crate::profiling::ProfilingMetadata;

        // Only some fields populated
        let profiling = ProfilingMetadata {
            source_ns: Some(1000),
            source_send_ns: Some(3000),
            query_receive_ns: Some(4000),
            ..Default::default()
        };

        let result = build_tracking_metadata(&profiling, 10);

        let tracking = result.get("tracking").unwrap().as_object().unwrap();

        // Verify source tracking - should have partial data
        let source = tracking.get("source").unwrap().as_object().unwrap();
        assert_eq!(
            source.get("changeDispatcherEnd_ns").unwrap().as_u64(),
            Some(3000)
        );
        assert_eq!(source.get("source_ns").unwrap().as_u64(), Some(1000));
        assert_eq!(source.get("seq").unwrap().as_u64(), Some(10));
        assert!(!source.contains_key("changeRouterStart_ns")); // Not set because source_receive_ns is None

        // Verify query tracking - should have partial data
        let query = tracking.get("query").unwrap().as_object().unwrap();
        assert_eq!(query.get("enqueue_ns").unwrap().as_u64(), Some(3000));
        assert_eq!(query.get("dequeue_ns").unwrap().as_u64(), Some(4000));
        assert!(!query.contains_key("queryStart_ns")); // Not set because query_core_call_ns is None
    }

    #[test]
    fn test_build_tracking_metadata_minimal_fields() {
        use crate::profiling::ProfilingMetadata;

        // Minimal profiling data - just sequence
        let profiling = ProfilingMetadata::default();

        let result = build_tracking_metadata(&profiling, 99);

        let tracking = result.get("tracking").unwrap().as_object().unwrap();

        // Should have source with just seq
        let source = tracking.get("source").unwrap().as_object().unwrap();
        assert_eq!(source.get("seq").unwrap().as_u64(), Some(99));
        assert_eq!(source.len(), 1); // Only seq field

        // Query might be empty or not present depending on implementation
        // If no query fields are set, the query object might still be present but empty
    }

    #[test]
    fn test_transform_with_profiling_metadata() {
        use crate::profiling::ProfilingMetadata;

        let profiling = ProfilingMetadata {
            source_ns: Some(1744055144490466971),
            source_receive_ns: Some(1744055159124143047),
            source_send_ns: Some(1744055173551481387),
            query_receive_ns: Some(1744055178510629042),
            query_core_call_ns: Some(1744055178510650750),
            query_core_return_ns: Some(1744055178510848750),
            ..Default::default()
        };

        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp: chrono::DateTime::from_timestamp_millis(1609459200000).unwrap(),
            results: vec![json!({"type": "add", "data": {"id": "1"}})],
            metadata: HashMap::new(),
            profiling: Some(profiling),
        };

        let result = transform_query_result(query_result, 24851, 24851).unwrap();

        match result {
            ResultEvent::Change(change) => {
                // Metadata should be present with tracking data
                assert!(change.metadata.is_some());
                let metadata = change.metadata.unwrap();

                // Verify tracking structure
                assert!(metadata.contains_key("tracking"));
                let tracking = metadata.get("tracking").unwrap().as_object().unwrap();

                // Verify source tracking exists
                assert!(tracking.contains_key("source"));
                let source = tracking.get("source").unwrap().as_object().unwrap();
                assert_eq!(source.get("seq").unwrap().as_u64(), Some(24851));
                assert_eq!(
                    source.get("source_ns").unwrap().as_u64(),
                    Some(1744055144490466971)
                );

                // Verify query tracking exists
                assert!(tracking.contains_key("query"));
                let query = tracking.get("query").unwrap().as_object().unwrap();
                assert_eq!(
                    query.get("enqueue_ns").unwrap().as_u64(),
                    Some(1744055173551481387)
                );
                assert_eq!(
                    query.get("dequeue_ns").unwrap().as_u64(),
                    Some(1744055178510629042)
                );
            }
            _ => panic!("Expected Change event"),
        }
    }

    #[test]
    fn test_transform_profiling_merged_with_existing_metadata() {
        use crate::profiling::ProfilingMetadata;

        let mut metadata = HashMap::new();
        metadata.insert("custom_field".to_string(), json!({"value": 123}));

        let profiling = ProfilingMetadata {
            source_ns: Some(1000),
            source_send_ns: Some(2000),
            ..Default::default()
        };

        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp: chrono::DateTime::from_timestamp_millis(1609459200000).unwrap(),
            results: vec![json!({"type": "add", "data": {"id": "1"}})],
            metadata,
            profiling: Some(profiling),
        };

        let result = transform_query_result(query_result, 1, 1).unwrap();

        match result {
            ResultEvent::Change(change) => {
                let meta = change.metadata.unwrap();

                // Should have both custom field and tracking
                assert!(meta.contains_key("custom_field"));
                assert!(meta.contains_key("tracking"));
            }
            _ => panic!("Expected Change event"),
        }
    }
}
