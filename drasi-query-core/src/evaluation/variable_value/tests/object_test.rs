use crate::evaluation::variable_value::VariableValue;
use serde_json::{json, Map};
use std::collections::BTreeMap;

#[test]
fn test_object_from_json() {
    let properties = json!({ "name": "Room 01_01_01", "comfortLevel": 50, "temp": 74, "humidity": 44, "co2": 500 });
    let value = VariableValue::from(properties);

    let mut tree_map = BTreeMap::new();
    tree_map.insert(
        "name".to_string(),
        VariableValue::String("Room 01_01_01".into()),
    );
    tree_map.insert(
        "comfortLevel".to_string(),
        VariableValue::Integer(50.into()),
    );
    tree_map.insert("temp".to_string(), VariableValue::Integer(74.into()));
    tree_map.insert("humidity".to_string(), VariableValue::Integer(44.into()));
    tree_map.insert("co2".to_string(), VariableValue::Integer(500.into()));

    assert_eq!(value, VariableValue::Object(tree_map));
    assert_eq!(
        *value.get("name").unwrap(),
        VariableValue::String("Room 01_01_01".into())
    );
}

#[test]
fn test_object_serde_map() {
    let serde_map = {
        let mut serde_map = Map::new();
        serde_map.insert("name".to_string(), json!("Room 01_01_01"));
        serde_map.insert("comfortLevel".to_string(), json!(50));
        serde_map.insert("temp".to_string(), json!(74));
        serde_map.insert("humidity".to_string(), json!(44));
        serde_map.insert("co2".to_string(), json!(500));
        serde_map
    };

    let value = VariableValue::from(serde_map);

    let mut tree_map = BTreeMap::new();
    tree_map.insert(
        "name".to_string(),
        VariableValue::String("Room 01_01_01".into()),
    );
    tree_map.insert(
        "comfortLevel".to_string(),
        VariableValue::Integer(50.into()),
    );
    tree_map.insert("temp".to_string(), VariableValue::Integer(74.into()));
    tree_map.insert("humidity".to_string(), VariableValue::Integer(44.into()));
    tree_map.insert("co2".to_string(), VariableValue::Integer(500.into()));

    assert_eq!(value, VariableValue::Object(tree_map));
    assert_eq!(
        *value.get("name").unwrap(),
        VariableValue::String("Room 01_01_01".into())
    );
}
