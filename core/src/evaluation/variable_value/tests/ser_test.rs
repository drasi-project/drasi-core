use crate::evaluation::variable_value::VariableValue;
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn test_serializing_integer() {
    let value = VariableValue::Integer(42.into());
    let ser_value = serde_json::to_string(&value).unwrap();
    assert_eq!(ser_value, json!(42).to_string());
}

#[test]
fn test_serializing_float() {
    let value = VariableValue::Float(42.0.into());
    let ser_value = serde_json::to_string(&value).unwrap();
    assert_eq!(ser_value, json!(42.0).to_string());
}

#[test]
fn test_serializing_string() {
    let value = VariableValue::String("drasi".into());
    let ser_value = serde_json::to_value(&value).unwrap();
    assert_eq!(ser_value, json!("drasi"));
}

#[test]
fn test_serializing_list() {
    let vec = vec![
        VariableValue::Integer(42.into()),
        VariableValue::Integer(43.into()),
        VariableValue::String("Drasi loves rust".to_string()),
    ];
    let alist = VariableValue::List(vec);
    let ser_value = serde_json::to_value(&alist).unwrap();
    assert_eq!(ser_value, json!([42, 43, "Drasi loves rust"]));
}

#[test]
fn test_serializing_object() {
    let tree_map = {
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
        tree_map
    };
    let obj = VariableValue::Object(tree_map);
    let ser_value = serde_json::to_value(&obj).unwrap();

    let expected = {
        let mut tree_map = BTreeMap::new();
        tree_map.insert("name".to_string(), json!("Room 01_01_01"));
        tree_map.insert("comfortLevel".to_string(), json!(50));
        tree_map.insert("temp".to_string(), json!(74));
        tree_map.insert("humidity".to_string(), json!(44));
        tree_map.insert("co2".to_string(), json!(500));
        tree_map
    };

    assert_eq!(ser_value, json!(expected));
}
