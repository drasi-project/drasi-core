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

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use ordered_float::OrderedFloat;
use std::sync::Arc;

use crate::rpc::{SuiEvent, SuiObjectData};

/// Enrichment configuration flags controlling which graph nodes and
/// relationships are emitted alongside the core event nodes.
#[derive(Debug, Clone, Copy)]
pub struct EnrichmentConfig {
    pub enable_pool_nodes: bool,
    pub enable_trader_nodes: bool,
    pub enable_order_nodes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventOperation {
    Insert,
    Update,
    Delete,
}

pub fn should_include_event(event: &SuiEvent, event_filters: &[String], pools: &[String]) -> bool {
    if !event_filters.is_empty() {
        let event_type_lower = event.event_type.to_ascii_lowercase();
        let has_match = event_filters.iter().any(|filter| {
            let filter_lower = filter.to_ascii_lowercase();
            event_type_lower.ends_with(&format!("::{filter_lower}"))
                || event_type_lower.contains(&filter_lower)
        });
        if !has_match {
            return false;
        }
    }

    if pools.is_empty() {
        return true;
    }

    let Some(pool_id) = extract_pool_id(&event.parsed_json) else {
        return false;
    };

    pools.iter().any(|p| p == &pool_id)
}

/// Maps a Sui event to a SourceChange using operation classification (insert/update/delete).
/// Used by the streaming source.
pub fn map_event_to_change(source_id: &str, event: &SuiEvent) -> SourceChange {
    let effective_from = event
        .timestamp_ms
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis().max(0) as u64);

    let entity_id = derive_entity_id(event);
    let operation = classify_operation(&event.event_type);
    let label = derive_secondary_label(event);

    let mut properties = build_common_properties(event, &entity_id);
    properties.insert(
        "change_type",
        ElementValue::String(Arc::from(match operation {
            EventOperation::Insert => "insert",
            EventOperation::Update => "update",
            EventOperation::Delete => "delete",
        })),
    );

    let labels: Arc<[Arc<str>]> =
        vec![Arc::from("DeepBookEvent"), Arc::from(label.as_str())].into();

    let metadata = ElementMetadata {
        reference: ElementReference::new(source_id, &entity_id),
        labels,
        effective_from,
    };

    match operation {
        EventOperation::Insert => SourceChange::Insert {
            element: Element::Node {
                metadata,
                properties,
            },
        },
        EventOperation::Update => SourceChange::Update {
            element: Element::Node {
                metadata,
                properties,
            },
        },
        EventOperation::Delete => SourceChange::Delete { metadata },
    }
}

/// Maps a Sui event to an Insert-only SourceChange.  Used by the bootstrap
/// provider where every historical event is treated as an insert.
pub fn map_event_to_insert_change(source_id: &str, event: &SuiEvent) -> SourceChange {
    let effective_from = event
        .timestamp_ms
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis().max(0) as u64);

    let entity_id = derive_entity_id(event);
    let label = derive_secondary_label(event);

    let mut properties = build_common_properties(event, &entity_id);
    properties.insert("change_type", ElementValue::String(Arc::from("insert")));

    let labels: Arc<[Arc<str>]> =
        vec![Arc::from("DeepBookEvent"), Arc::from(label.as_str())].into();

    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, &entity_id),
                labels,
                effective_from,
            },
            properties,
        },
    }
}

// ---------------------------------------------------------------------------
// Enrichment node & relationship builders
// ---------------------------------------------------------------------------

/// Build a `:Pool` node from `sui_getObject` data.
pub fn build_pool_node(
    source_id: &str,
    pool_id: &str,
    object_data: Option<&SuiObjectData>,
    effective_from: u64,
) -> SourceChange {
    let entity_id = format!("pool_meta:{pool_id}");
    let mut properties = ElementPropertyMap::new();
    properties.insert("pool_id", ElementValue::String(Arc::from(pool_id)));
    properties.insert(
        "pool_id_short",
        ElementValue::String(Arc::from(truncate_hex(pool_id))),
    );

    if let Some(data) = object_data {
        let type_params = data.type_params();
        if let Some(base) = type_params.first() {
            properties.insert("base_asset", ElementValue::String(Arc::from(base.as_str())));
        }
        if let Some(quote) = type_params.get(1) {
            properties.insert(
                "quote_asset",
                ElementValue::String(Arc::from(quote.as_str())),
            );
        }
        if let Some(tick_size) = data.field_str("tick_size") {
            properties.insert(
                "tick_size",
                ElementValue::String(Arc::from(tick_size.as_str())),
            );
        }
        if let Some(lot_size) = data.field_str("lot_size") {
            properties.insert(
                "lot_size",
                ElementValue::String(Arc::from(lot_size.as_str())),
            );
        }
        if let Some(min_size) = data.field_str("min_size") {
            properties.insert(
                "min_size",
                ElementValue::String(Arc::from(min_size.as_str())),
            );
        }
    }

    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, &entity_id),
                labels: Arc::new([Arc::from("Pool")]),
                effective_from,
            },
            properties,
        },
    }
}

/// Build a `:Trader` node from a sender address.
pub fn build_trader_node(source_id: &str, address: &str, effective_from: u64) -> SourceChange {
    let entity_id = format!("trader:{address}");
    let mut properties = ElementPropertyMap::new();
    properties.insert("address", ElementValue::String(Arc::from(address)));
    properties.insert(
        "address_short",
        ElementValue::String(Arc::from(truncate_hex(address))),
    );

    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, &entity_id),
                labels: Arc::new([Arc::from("Trader")]),
                effective_from,
            },
            properties,
        },
    }
}

/// Build an `:Order` node from an order_id.
pub fn build_order_node(
    source_id: &str,
    order_id: &str,
    pool_id: Option<&str>,
    effective_from: u64,
) -> SourceChange {
    let entity_id = format!("order_meta:{order_id}");
    let mut properties = ElementPropertyMap::new();
    properties.insert("order_id", ElementValue::String(Arc::from(order_id)));
    properties.insert(
        "order_id_short",
        ElementValue::String(Arc::from(truncate_hex(order_id))),
    );
    if let Some(pool) = pool_id {
        properties.insert("pool_id", ElementValue::String(Arc::from(pool)));
    }

    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, &entity_id),
                labels: Arc::new([Arc::from("Order")]),
                effective_from,
            },
            properties,
        },
    }
}

/// Build a relationship edge between two nodes.
pub fn build_relationship(
    source_id: &str,
    label: &str,
    rel_entity_id: &str,
    in_node_id: &str,
    out_node_id: &str,
    effective_from: u64,
) -> SourceChange {
    SourceChange::Insert {
        element: Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, rel_entity_id),
                labels: Arc::new([Arc::from(label)]),
                effective_from,
            },
            properties: ElementPropertyMap::new(),
            in_node: ElementReference::new(source_id, in_node_id),
            out_node: ElementReference::new(source_id, out_node_id),
        },
    }
}

/// Helper to extract `pool_id` from an event (public for use by enrichment logic).
pub fn event_pool_id(event: &SuiEvent) -> Option<String> {
    extract_pool_id(&event.parsed_json)
}

/// Helper to extract `order_id` from an event (public for use by enrichment logic).
pub fn event_order_id(event: &SuiEvent) -> Option<String> {
    extract_string_field(&event.parsed_json, &["order_id", "orderId"])
}

fn build_common_properties(event: &SuiEvent, entity_id: &str) -> ElementPropertyMap {
    let mut properties = ElementPropertyMap::new();
    properties.insert("entity_id", ElementValue::String(Arc::from(entity_id)));
    properties.insert(
        "tx_digest",
        ElementValue::String(Arc::from(event.id.tx_digest.as_str())),
    );
    properties.insert(
        "event_seq",
        ElementValue::String(Arc::from(event.id.event_seq.as_str())),
    );
    properties.insert(
        "package_id",
        ElementValue::String(Arc::from(event.package_id.as_str())),
    );
    properties.insert(
        "transaction_module",
        ElementValue::String(Arc::from(event.transaction_module.as_str())),
    );
    properties.insert(
        "sender",
        ElementValue::String(Arc::from(event.sender.as_str())),
    );
    properties.insert(
        "event_type",
        ElementValue::String(Arc::from(event.event_type.as_str())),
    );
    // Short human-readable name extracted from the Move type path
    // e.g. "0x337…::deep_price::PriceAdded<0x2::sui::SUI>" → "PriceAdded"
    let base_type = strip_type_params(&event.event_type);
    let event_name = base_type.split("::").last().unwrap_or("Event");
    properties.insert("event_name", ElementValue::String(Arc::from(event_name)));
    // Module that emitted the event, e.g. "deep_price", "balance_manager"
    let module_name = base_type
        .split("::")
        .nth(1)
        .unwrap_or(&event.transaction_module);
    properties.insert("module", ElementValue::String(Arc::from(module_name)));
    properties.insert(
        "sender_short",
        ElementValue::String(Arc::from(truncate_hex(&event.sender))),
    );
    properties.insert(
        "timestamp_ms",
        ElementValue::Integer(event.timestamp_ms.unwrap_or_default() as i64),
    );
    properties.insert("payload", json_value_to_element_value(&event.parsed_json));

    if let Some(order_id) = extract_string_field(&event.parsed_json, &["order_id", "orderId"]) {
        properties.insert(
            "order_id",
            ElementValue::String(Arc::from(order_id.as_str())),
        );
    }
    if let Some(pool_id) = extract_pool_id(&event.parsed_json) {
        properties.insert("pool_id", ElementValue::String(Arc::from(pool_id.as_str())));
        properties.insert(
            "pool_id_short",
            ElementValue::String(Arc::from(truncate_hex(&pool_id))),
        );
    }

    properties
}

/// Strips Move generic type parameters from an event type string.
/// e.g. `"0xdee9::clob_v2::OrderPlaced<0x2::sui::SUI, ...>"` → `"0xdee9::clob_v2::OrderPlaced"`
fn strip_type_params(event_type: &str) -> &str {
    event_type.split('<').next().unwrap_or(event_type)
}

fn classify_operation(event_type: &str) -> EventOperation {
    let event_type = event_type.to_ascii_lowercase();
    if event_type.contains("cancel")
        || event_type.contains("delete")
        || event_type.contains("remove")
    {
        return EventOperation::Delete;
    }
    if event_type.contains("fill")
        || event_type.contains("update")
        || event_type.contains("modify")
        || event_type.contains("amend")
    {
        return EventOperation::Update;
    }
    EventOperation::Insert
}

/// Public accessor for the entity ID derivation logic.
pub fn derive_entity_id_pub(event: &SuiEvent) -> String {
    derive_entity_id(event)
}

fn derive_entity_id(event: &SuiEvent) -> String {
    if let Some(order_id) = extract_string_field(&event.parsed_json, &["order_id", "orderId"]) {
        return format!("order:{order_id}");
    }
    if let Some(pool_id) = extract_pool_id(&event.parsed_json) {
        return format!("pool:{pool_id}");
    }
    format!("event:{}:{}", event.id.tx_digest, event.id.event_seq)
}

fn derive_secondary_label(event: &SuiEvent) -> String {
    let base_type = strip_type_params(&event.event_type);
    let name = base_type
        .split("::")
        .last()
        .unwrap_or("Event")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();

    if name.is_empty() {
        "Event".to_string()
    } else {
        name
    }
}

fn extract_pool_id(parsed_json: &serde_json::Value) -> Option<String> {
    extract_string_field(parsed_json, &["pool_id", "poolId", "pool", "reference_pool", "target_pool"])
}

fn extract_string_field(parsed_json: &serde_json::Value, keys: &[&str]) -> Option<String> {
    let serde_json::Value::Object(map) = parsed_json else {
        return None;
    };
    for key in keys {
        let Some(value) = map.get(*key) else {
            continue;
        };
        let converted = match value {
            serde_json::Value::String(text) => Some(text.clone()),
            serde_json::Value::Number(num) => Some(num.to_string()),
            serde_json::Value::Bool(value) => Some(value.to_string()),
            _ => None,
        };
        if converted.is_some() {
            return converted;
        }
    }
    None
}

fn json_value_to_element_value(value: &serde_json::Value) -> ElementValue {
    match value {
        serde_json::Value::Null => ElementValue::Null,
        serde_json::Value::Bool(v) => ElementValue::Bool(*v),
        serde_json::Value::Number(v) => {
            if let Some(i) = v.as_i64() {
                ElementValue::Integer(i)
            } else if let Some(f) = v.as_f64() {
                ElementValue::Float(OrderedFloat(f))
            } else {
                ElementValue::String(Arc::from(v.to_string()))
            }
        }
        serde_json::Value::String(v) => ElementValue::String(Arc::from(v.as_str())),
        serde_json::Value::Array(items) => ElementValue::List(
            items
                .iter()
                .map(json_value_to_element_value)
                .collect::<Vec<_>>(),
        ),
        serde_json::Value::Object(map) => {
            let mut props = ElementPropertyMap::new();
            for (key, value) in map {
                props.insert(key, json_value_to_element_value(value));
            }
            ElementValue::Object(props)
        }
    }
}

/// Truncate a hex address to `0x1234…abcd` form for readable output.
fn truncate_hex(hex: &str) -> String {
    if hex.len() <= 12 {
        return hex.to_string();
    }
    let prefix = &hex[..6]; // "0x" + 4 chars
    let suffix = &hex[hex.len() - 4..];
    format!("{prefix}…{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{EventCursor, SuiEvent};

    fn event_with_type(event_type: &str, parsed_json: serde_json::Value) -> SuiEvent {
        SuiEvent {
            id: EventCursor {
                tx_digest: "0xtx".to_string(),
                event_seq: "1".to_string(),
            },
            package_id: "0xpackage".to_string(),
            transaction_module: "pool".to_string(),
            sender: "0xsender".to_string(),
            event_type: event_type.to_string(),
            parsed_json,
            timestamp_ms: Some(1_772_923_888_171),
        }
    }

    #[test]
    fn test_classifies_delete_events() {
        let event = event_with_type(
            "0x1::events::OrderCancelled",
            serde_json::json!({"order_id": "42"}),
        );
        let change = map_event_to_change("source-a", &event);
        assert!(matches!(change, SourceChange::Delete { .. }));
    }

    #[test]
    fn test_maps_insert_order_event() {
        let event = event_with_type(
            "0x1::events::OrderPlaced",
            serde_json::json!({"order_id": "42", "price": "12.34"}),
        );
        let change = map_event_to_change("source-a", &event);
        match change {
            SourceChange::Insert {
                element:
                    Element::Node {
                        metadata,
                        properties,
                    },
            } => {
                assert_eq!(metadata.reference.element_id.as_ref(), "order:42");
                assert_eq!(
                    properties["change_type"],
                    ElementValue::String(Arc::from("insert"))
                );
            }
            _ => panic!("expected insert node"),
        }
    }

    #[test]
    fn test_filter_by_event_type_and_pool() {
        let event = event_with_type(
            "0x1::events::OrderPlaced",
            serde_json::json!({"order_id": "42", "pool_id": "pool-a"}),
        );
        assert!(should_include_event(
            &event,
            &[String::from("OrderPlaced")],
            &[String::from("pool-a")]
        ));
        assert!(!should_include_event(
            &event,
            &[String::from("OrderFilled")],
            &[String::from("pool-a")]
        ));
        assert!(!should_include_event(
            &event,
            &[String::from("OrderPlaced")],
            &[String::from("pool-b")]
        ));
    }

    #[test]
    fn test_build_pool_node_with_object_data() {
        use crate::rpc::{SuiObjectContent, SuiObjectData};

        let object_data = SuiObjectData {
            object_id: "0xpool_abc".to_string(),
            object_type: Some("0xdee9::pool::Pool<0x2::sui::SUI, 0xabc::usdc::USDC>".to_string()),
            content: Some(SuiObjectContent {
                data_type: "moveObject".to_string(),
                fields: Some(serde_json::json!({
                    "tick_size": "1000000",
                    "lot_size": "100000000",
                    "min_size": "500000000"
                })),
            }),
        };

        let change = build_pool_node("src-1", "0xpool_abc", Some(&object_data), 100);
        match change {
            SourceChange::Insert {
                element:
                    Element::Node {
                        metadata,
                        properties,
                    },
            } => {
                assert_eq!(
                    metadata.reference.element_id.as_ref(),
                    "pool_meta:0xpool_abc"
                );
                assert_eq!(metadata.labels[0].as_ref(), "Pool");
                assert_eq!(
                    properties["base_asset"],
                    ElementValue::String(Arc::from("0x2::sui::SUI"))
                );
                assert_eq!(
                    properties["quote_asset"],
                    ElementValue::String(Arc::from("0xabc::usdc::USDC"))
                );
                assert_eq!(
                    properties["tick_size"],
                    ElementValue::String(Arc::from("1000000"))
                );
            }
            _ => panic!("expected insert node"),
        }
    }

    #[test]
    fn test_build_pool_node_without_object_data() {
        let change = build_pool_node("src-1", "0xpool_abc", None, 100);
        match change {
            SourceChange::Insert {
                element:
                    Element::Node {
                        metadata,
                        properties,
                    },
            } => {
                assert_eq!(
                    metadata.reference.element_id.as_ref(),
                    "pool_meta:0xpool_abc"
                );
                assert_eq!(
                    properties["pool_id"],
                    ElementValue::String(Arc::from("0xpool_abc"))
                );
                assert!(properties.get("base_asset").is_none());
            }
            _ => panic!("expected insert node"),
        }
    }

    #[test]
    fn test_build_trader_node() {
        let change = build_trader_node("src-1", "0xtrader_123456789abcdef", 200);
        match change {
            SourceChange::Insert {
                element:
                    Element::Node {
                        metadata,
                        properties,
                    },
            } => {
                assert_eq!(
                    metadata.reference.element_id.as_ref(),
                    "trader:0xtrader_123456789abcdef"
                );
                assert_eq!(metadata.labels[0].as_ref(), "Trader");
                assert_eq!(
                    properties["address"],
                    ElementValue::String(Arc::from("0xtrader_123456789abcdef"))
                );
            }
            _ => panic!("expected insert node"),
        }
    }

    #[test]
    fn test_build_order_node() {
        let change = build_order_node("src-1", "42", Some("0xpool_abc"), 300);
        match change {
            SourceChange::Insert {
                element:
                    Element::Node {
                        metadata,
                        properties,
                    },
            } => {
                assert_eq!(metadata.reference.element_id.as_ref(), "order_meta:42");
                assert_eq!(metadata.labels[0].as_ref(), "Order");
                assert_eq!(
                    properties["order_id"],
                    ElementValue::String(Arc::from("42"))
                );
                assert_eq!(
                    properties["pool_id"],
                    ElementValue::String(Arc::from("0xpool_abc"))
                );
            }
            _ => panic!("expected insert node"),
        }
    }

    #[test]
    fn test_build_relationship() {
        let change = build_relationship(
            "src-1",
            "IN_POOL",
            "rel:in_pool:order:42",
            "order:42",
            "pool_meta:0xpool_abc",
            100,
        );
        match change {
            SourceChange::Insert {
                element:
                    Element::Relation {
                        metadata,
                        in_node,
                        out_node,
                        ..
                    },
            } => {
                assert_eq!(
                    metadata.reference.element_id.as_ref(),
                    "rel:in_pool:order:42"
                );
                assert_eq!(metadata.labels[0].as_ref(), "IN_POOL");
                assert_eq!(in_node.element_id.as_ref(), "order:42");
                assert_eq!(out_node.element_id.as_ref(), "pool_meta:0xpool_abc");
            }
            _ => panic!("expected insert relation"),
        }
    }

    #[test]
    fn test_event_pool_id_and_order_id() {
        let event = event_with_type(
            "0x1::events::OrderPlaced",
            serde_json::json!({"order_id": "42", "pool_id": "0xpool_abc"}),
        );
        assert_eq!(event_pool_id(&event), Some("0xpool_abc".to_string()));
        assert_eq!(event_order_id(&event), Some("42".to_string()));

        let event_no_ids = event_with_type(
            "0x1::events::BalanceEvent",
            serde_json::json!({"amount": "100"}),
        );
        assert_eq!(event_pool_id(&event_no_ids), None);
        assert_eq!(event_order_id(&event_no_ids), None);
    }

    #[test]
    fn test_generic_event_type_parsing() {
        // Real DeepBook event types include Move generic parameters
        let event = event_with_type(
            "0xdee9::clob_v2::OrderPlaced<0x2::sui::SUI, 0x5d4b::coin::COIN>",
            serde_json::json!({"order_id": "99", "pool_id": "0xpool", "price": "1.5"}),
        );
        let change = map_event_to_change("src", &event);
        match change {
            SourceChange::Insert {
                element: Element::Node { properties, .. },
            } => {
                // event_name must be "OrderPlaced", NOT "COIN>"
                assert_eq!(
                    properties["event_name"],
                    ElementValue::String(Arc::from("OrderPlaced"))
                );
                // module must be "clob_v2", NOT something from generics
                assert_eq!(
                    properties["module"],
                    ElementValue::String(Arc::from("clob_v2"))
                );
            }
            _ => panic!("expected insert node"),
        }
    }

    #[test]
    fn test_strip_type_params() {
        assert_eq!(strip_type_params("0x1::mod::Foo<0x2::bar::BAZ>"), "0x1::mod::Foo");
        assert_eq!(strip_type_params("0x1::mod::Foo"), "0x1::mod::Foo");
        assert_eq!(strip_type_params("Foo<A, B>"), "Foo");
    }
}
