// Copyright 2024 The Drasi Authors.
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
