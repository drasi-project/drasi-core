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

use crate::config::{Neo4jSourceConfig, StartCursor};
use crate::mapping::bolt_map_to_element_properties;
use anyhow::{anyhow, Result};
use chrono::Utc;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::state_store::StateStoreProvider;
use neo4rs::{query, BoltList, BoltMap, BoltType, Graph};
use std::collections::HashMap;
use std::sync::Arc;

const CURSOR_STATE_KEY: &str = "neo4j_cdc_state";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Neo4jCdcState {
    pub cursor: String,
    pub database: String,
}

pub fn build_selectors(labels: &[String], rel_types: &[String]) -> Vec<HashMap<String, BoltType>> {
    let mut selectors = Vec::new();

    for label in labels {
        let mut selector = HashMap::new();
        selector.insert("select".to_string(), BoltType::String("n".into()));
        selector.insert(
            "labels".to_string(),
            BoltType::List(BoltList::from(vec![BoltType::String(label.clone().into())])),
        );
        selectors.push(selector);
    }

    for rel_type in rel_types {
        let mut selector = HashMap::new();
        selector.insert("select".to_string(), BoltType::String("r".into()));
        selector.insert(
            "type".to_string(),
            BoltType::String(rel_type.clone().into()),
        );
        selectors.push(selector);
    }

    selectors
}

pub async fn determine_start_cursor(
    graph: &Graph,
    source_id: &str,
    config: &Neo4jSourceConfig,
    state_store: Option<Arc<dyn StateStoreProvider>>,
) -> Result<String> {
    if let Some(state_store) = state_store {
        match state_store.get(source_id, CURSOR_STATE_KEY).await {
            Ok(Some(raw)) => {
                let state: Neo4jCdcState = serde_json::from_slice(&raw)?;
                if state.database == config.database {
                    return Ok(state.cursor);
                }
            }
            Ok(None) => {}
            Err(e) => {
                log::warn!("Failed to read CDC cursor from state store: {e}");
            }
        }
    }

    match config.start_cursor {
        StartCursor::Now => {
            query_single_cursor(graph, "CALL db.cdc.current() YIELD id RETURN id").await
        }
        StartCursor::Beginning => {
            query_single_cursor(graph, "CALL db.cdc.earliest() YIELD id RETURN id").await
        }
        StartCursor::Timestamp(_) => {
            // Neo4j CDC doesn't expose direct seek-by-timestamp. Start from earliest and
            // let the processing loop apply timestamp filtering if configured.
            query_single_cursor(graph, "CALL db.cdc.earliest() YIELD id RETURN id").await
        }
    }
}

pub async fn persist_cursor(
    source_id: &str,
    database: &str,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    cursor: &str,
) {
    if let Some(state_store) = state_store {
        let state = Neo4jCdcState {
            cursor: cursor.to_string(),
            database: database.to_string(),
        };

        match serde_json::to_vec(&state) {
            Ok(payload) => {
                if let Err(e) = state_store.set(source_id, CURSOR_STATE_KEY, payload).await {
                    log::warn!("Failed to persist CDC cursor to state store: {e}");
                }
            }
            Err(e) => {
                log::warn!("Failed to serialize CDC cursor state: {e}");
            }
        }
    }
}

async fn query_single_cursor(graph: &Graph, cypher: &str) -> Result<String> {
    let mut stream = graph.execute(query(cypher)).await?;
    match stream.next().await {
        Ok(Some(row)) => Ok(row.get::<String>("id")?),
        Ok(None) => Err(anyhow!("Neo4j CDC cursor query returned no rows")),
        Err(e) => Err(anyhow!("Neo4j CDC cursor query failed: {e}")),
    }
}

pub fn parse_source_change(
    source_id: &str,
    event_map: &BoltMap,
    now_ms: u64,
) -> Result<Option<SourceChange>> {
    let event_type = get_string(event_map, "eventType")
        .or_else(|| get_string(event_map, "event_type"))
        .unwrap_or_default()
        .to_lowercase();
    let operation = get_string(event_map, "operation")
        .unwrap_or_else(|| "u".to_string())
        .to_lowercase();

    let element_id = match get_string(event_map, "elementId") {
        Some(id) => id,
        None => return Ok(None),
    };

    let is_delete = operation == "d" || operation == "delete";
    let metadata = ElementMetadata {
        reference: ElementReference::new(source_id, &element_id),
        labels: extract_labels(event_map),
        effective_from: now_ms,
    };

    if event_type == "n" || event_type == "node" {
        if is_delete {
            return Ok(Some(SourceChange::Delete { metadata }));
        }

        let properties = extract_properties(event_map, true)?;
        let element = Element::Node {
            metadata,
            properties,
        };

        if operation == "c" || operation == "create" {
            Ok(Some(SourceChange::Insert { element }))
        } else {
            Ok(Some(SourceChange::Update { element }))
        }
    } else if event_type == "r" || event_type == "relationship" {
        if is_delete {
            return Ok(Some(SourceChange::Delete { metadata }));
        }

        let start_node_id = get_string(event_map, "startNodeElementId")
            .or_else(|| get_nested_string(event_map, "start", "elementId"))
            .ok_or_else(|| anyhow!("Relationship CDC event missing startNodeElementId"))?;
        let end_node_id = get_string(event_map, "endNodeElementId")
            .or_else(|| get_nested_string(event_map, "end", "elementId"))
            .ok_or_else(|| anyhow!("Relationship CDC event missing endNodeElementId"))?;

        let properties = extract_properties(event_map, true)?;

        let element = Element::Relation {
            metadata,
            properties,
            in_node: ElementReference::new(source_id, &start_node_id),
            out_node: ElementReference::new(source_id, &end_node_id),
        };

        if operation == "c" || operation == "create" {
            Ok(Some(SourceChange::Insert { element }))
        } else {
            Ok(Some(SourceChange::Update { element }))
        }
    } else {
        Ok(None)
    }
}

fn extract_labels(event_map: &BoltMap) -> Arc<[Arc<str>]> {
    let mut labels = Vec::<Arc<str>>::new();

    if let Some(rel_type) =
        get_string(event_map, "type").or_else(|| get_string(event_map, "relationshipType"))
    {
        labels.push(Arc::from(rel_type));
    } else if let Some(list) = get_list(event_map, "labels") {
        labels.extend(bolt_list_to_labels(list));
    } else if let Some(state) = get_map(event_map, "state") {
        for side in ["after", "before"] {
            if let Some(entity) = get_map(state, side) {
                if let Some(list) = get_list(entity, "labels") {
                    labels.extend(bolt_list_to_labels(list));
                    break;
                }
            }
        }
    }

    if labels.is_empty() {
        labels.push(Arc::from("entity"));
    }
    labels.into()
}

fn extract_properties(event_map: &BoltMap, prefer_after: bool) -> Result<ElementPropertyMap> {
    if let Some(state) = get_map(event_map, "state") {
        let order = if prefer_after {
            ["after", "before"]
        } else {
            ["before", "after"]
        };
        for side in order {
            if let Some(entity) = get_map(state, side) {
                if let Some(props) = get_map(entity, "properties") {
                    return bolt_map_to_element_properties(props);
                }
            }
        }
    }

    Ok(ElementPropertyMap::new())
}

fn bolt_list_to_labels(list: &BoltList) -> Vec<Arc<str>> {
    list.value
        .iter()
        .filter_map(|item| match item {
            BoltType::String(v) => Some(Arc::from(v.value.as_str())),
            _ => None,
        })
        .collect()
}

fn get_map<'a>(map: &'a BoltMap, key: &str) -> Option<&'a BoltMap> {
    match map.value.get(key) {
        Some(BoltType::Map(v)) => Some(v),
        _ => None,
    }
}

fn get_list<'a>(map: &'a BoltMap, key: &str) -> Option<&'a BoltList> {
    match map.value.get(key) {
        Some(BoltType::List(v)) => Some(v),
        _ => None,
    }
}

fn get_string(map: &BoltMap, key: &str) -> Option<String> {
    match map.value.get(key) {
        Some(BoltType::String(v)) => Some(v.value.clone()),
        Some(v) => Some(v.to_string()),
        None => None,
    }
}

fn get_nested_string(map: &BoltMap, map_key: &str, value_key: &str) -> Option<String> {
    get_map(map, map_key).and_then(|nested| get_string(nested, value_key))
}

pub fn now_ms() -> u64 {
    Utc::now().timestamp_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use neo4rs::{BoltList, BoltMap, BoltType};

    fn make_node_event(operation: &str) -> BoltMap {
        let mut event = BoltMap::new();
        event.put("eventType".into(), BoltType::String("n".into()));
        event.put("operation".into(), BoltType::String(operation.into()));
        event.put("elementId".into(), BoltType::String("4:test:1".into()));
        event.put(
            "labels".into(),
            BoltType::List(BoltList::from(vec![BoltType::String("Person".into())])),
        );

        let mut after = BoltMap::new();
        let mut props = BoltMap::new();
        props.put("name".into(), BoltType::String("Alice".into()));
        after.put("properties".into(), BoltType::Map(props));

        let mut state = BoltMap::new();
        state.put("after".into(), BoltType::Map(after));
        event.put("state".into(), BoltType::Map(state));
        event
    }

    fn make_rel_event(operation: &str) -> BoltMap {
        let mut event = BoltMap::new();
        event.put("eventType".into(), BoltType::String("r".into()));
        event.put("operation".into(), BoltType::String(operation.into()));
        event.put("elementId".into(), BoltType::String("5:test:1".into()));
        event.put("type".into(), BoltType::String("ACTED_IN".into()));
        event.put(
            "startNodeElementId".into(),
            BoltType::String("4:test:start".into()),
        );
        event.put(
            "endNodeElementId".into(),
            BoltType::String("4:test:end".into()),
        );
        event
    }

    fn make_rel_event_nested(operation: &str) -> BoltMap {
        let mut event = BoltMap::new();
        event.put("eventType".into(), BoltType::String("r".into()));
        event.put("operation".into(), BoltType::String(operation.into()));
        event.put("elementId".into(), BoltType::String("5:test:1".into()));
        event.put(
            "relationshipType".into(),
            BoltType::String("ACTED_IN".into()),
        );

        let mut start = BoltMap::new();
        start.put("elementId".into(), BoltType::String("4:test:start".into()));
        event.put("start".into(), BoltType::Map(start));

        let mut end = BoltMap::new();
        end.put("elementId".into(), BoltType::String("4:test:end".into()));
        event.put("end".into(), BoltType::Map(end));
        event
    }

    #[test]
    fn test_parse_node_create() {
        let event = make_node_event("c");
        let change = parse_source_change("src", &event, 100).unwrap().unwrap();
        assert!(matches!(change, SourceChange::Insert { .. }));
    }

    #[test]
    fn test_parse_node_update() {
        let event = make_node_event("u");
        let change = parse_source_change("src", &event, 100).unwrap().unwrap();
        assert!(matches!(change, SourceChange::Update { .. }));
    }

    #[test]
    fn test_parse_node_delete() {
        let event = make_node_event("d");
        let change = parse_source_change("src", &event, 100).unwrap().unwrap();
        assert!(matches!(change, SourceChange::Delete { .. }));
    }

    #[test]
    fn test_parse_rel_create() {
        let event = make_rel_event("c");
        let change = parse_source_change("src", &event, 100).unwrap().unwrap();
        match change {
            SourceChange::Insert {
                element:
                    Element::Relation {
                        in_node, out_node, ..
                    },
            } => {
                assert_eq!(in_node.element_id.as_ref(), "4:test:start");
                assert_eq!(out_node.element_id.as_ref(), "4:test:end");
            }
            other => panic!("unexpected change: {other:?}"),
        }
    }

    #[test]
    fn test_parse_rel_delete() {
        let event = make_rel_event("d");
        let change = parse_source_change("src", &event, 100).unwrap().unwrap();
        assert!(matches!(change, SourceChange::Delete { .. }));
    }

    #[test]
    fn test_parse_rel_create_with_nested_start_end() {
        let event = make_rel_event_nested("c");
        let change = parse_source_change("src", &event, 100).unwrap().unwrap();
        assert!(matches!(change, SourceChange::Insert { .. }));
    }
}
